//! TLS/public dispatch and isolated management health with bounded draining.
use crate::{
    Body, Error,
    config::Config,
    legacy::{Failure, Legacy, TimedBody},
    ownership::{Context, Dispatcher},
    response,
};
use http_body_util::BodyExt;
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpListener,
    sync::watch,
    task::JoinSet,
    time::{Sleep, timeout},
};
use tokio_rustls::TlsAcceptor;

// Below TLS: accepting plaintext into TLS buffers is not socket write progress.
struct WriteDeadline<Io> {
    inner: Io,
    idle: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<Io> WriteDeadline<Io> {
    fn new(inner: Io, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            stalled: None,
        }
    }

    fn expired(&mut self, context: &mut TaskContext<'_>) -> bool {
        self.stalled
            .as_mut()
            .is_some_and(|timer| timer.as_mut().poll(context).is_ready())
    }

    fn track<T>(
        &mut self,
        context: &mut TaskContext<'_>,
        result: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if result.is_pending() {
            let timer = self
                .stalled
                .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.idle)));
            if timer.as_mut().poll(context).is_ready() {
                return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
            }
        } else {
            self.stalled = None;
        }
        result
    }
}

impl<Io: AsyncRead + Unpin> AsyncRead for WriteDeadline<Io> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl<Io: AsyncWrite + Unpin> AsyncWrite for WriteDeadline<Io> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.expired(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        let result = Pin::new(&mut self.inner).poll_write(context, bytes);
        // A zero-byte write is not progress (the caller handles WriteZero).
        if matches!(result, Poll::Ready(Ok(0))) {
            return result;
        }
        self.track(context, result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if self.expired(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        let result = Pin::new(&mut self.inner).poll_flush(context);
        self.track(context, result)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        if self.expired(context) {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        let result = Pin::new(&mut self.inner).poll_shutdown(context);
        self.track(context, result)
    }
}

/// Shared dispatcher exercised by production and boundary tests.
pub async fn dispatch(
    request: Request<Body>,
    context: Context,
    dispatcher: &Dispatcher,
    legacy: &Legacy,
) -> (Response<Body>, &'static str, Option<&'static str>) {
    if let Some(owner) = dispatcher.select(request.uri().path()) {
        return match (owner.handler)(request, context).await {
            Ok(response) => (response, owner.id, None),
            Err(_) => (response(500, "slice failure"), owner.id, Some("slice")),
        };
    }
    match legacy.send(request).await {
        Ok(response) => (response, "legacy", None),
        Err(error) => {
            let (status, category) = match error {
                Failure::Timeout => (504, "timeout"),
                Failure::TooLarge => (413, "body_limit"),
                Failure::Tls => (502, "tls"),
                Failure::Connection => (502, "connection"),
                Failure::Protocol => (502, "protocol"),
            };
            (
                response(status, "gateway transport failure"),
                "legacy",
                Some(category),
            )
        }
    }
}

/// Bind only after caller validates config/registry; drain until shutdown deadline.
pub async fn run(
    config: Config,
    dispatcher: Dispatcher,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Error> {
    let public = TcpListener::bind(config.public_bind).await?;
    let management = TcpListener::bind(config.management_bind).await?;
    let legacy = Legacy::new(config.clone());
    let dispatcher = Arc::new(dispatcher);
    let acceptor = TlsAcceptor::from(config.server_tls.clone());
    let counter = Arc::new(AtomicU64::new(1));
    let (stop, stopped) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = public.accept() => {
                let (stream, _) = accepted?;
                let (config, legacy, dispatcher, acceptor, counter, mut stopped) = (config.clone(), legacy.clone(), dispatcher.clone(), acceptor.clone(), counter.clone(), stopped.clone());
                tasks.spawn(async move {
                    let stream = WriteDeadline::new(stream, config.body_idle);
                    let Ok(Ok(stream)) = timeout(config.connect, acceptor.accept(stream)).await else { return; };
                    let service_config = config.clone();
                    let service = service_fn(move |request: Request<Incoming>| {
                        let (config, legacy, dispatcher, counter) = (service_config.clone(), legacy.clone(), dispatcher.clone(), counter.clone());
                        async move {
                            let started = Instant::now();
                            let request_id = counter.fetch_add(1, Ordering::Relaxed);
                            // Methods are client-controlled tokens; do not log arbitrary extensions.
                            let method = match request.method().as_str() { "GET" => "GET", "POST" => "POST", "HEAD" => "HEAD", "PUT" => "PUT", "DELETE" => "DELETE", "OPTIONS" => "OPTIONS", "PATCH" => "PATCH", _ => "OTHER" };
                            let request = request.map(|incoming| TimedBody::wrap(incoming.map_err(|error| -> Error { error.into() }).boxed_unsync(), config.body_idle, Some(config.request_body)));
                            let (reply, owner, category) = dispatch(request, Context { request_id }, &dispatcher, &legacy).await;
                            if config.log_level != "off" && (category.is_some() || ["info", "debug", "trace"].contains(&config.log_level.as_str())) {
                                eprintln!("request_id={request_id} owner={owner} method={method} status={} duration_ms={} error={}", reply.status().as_u16(), started.elapsed().as_millis(), category.unwrap_or("none"));
                            }
                            Ok::<_, Infallible>(reply)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(config.header).max_buf_size(65536);
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stopped.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; }
                    }
                });
            },
            accepted = management.accept() => {
                let (stream, _) = accepted?;
                let (legacy, mut stopped, config) = (legacy.clone(), stopped.clone(), config.clone());
                tasks.spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let legacy = legacy.clone();
                        async move {
                            let reply = match (request.method().as_str(), request.uri().path()) {
                                ("GET", "/live") => response(200, "live"),
                                ("GET", "/ready") => {
                                    if legacy.ready().await { response(200, "ready") }
                                    else { response(503, "not ready") }
                                },
                                _ => response(404, "not found"),
                            };
                            Ok::<_, Infallible>(reply)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(config.header).keep_alive(false).max_buf_size(8192);
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! { _ = &mut connection => {}, _ = stopped.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; } }
                });
            }
        }
    }
    stop.send_replace(true);
    if timeout(config.shutdown, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    struct FinalFlush;
    impl AsyncWrite for FinalFlush {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn stalled_final_flush_and_shutdown_expire_without_body_polls() {
        for shutdown in [false, true] {
            let mut stream = WriteDeadline::new(FinalFlush, Duration::from_millis(30));
            stream.write_all(b"final frame").await.unwrap();
            let error = timeout(Duration::from_secs(1), async {
                if shutdown {
                    stream.shutdown().await
                } else {
                    stream.flush().await
                }
            })
            .await
            .unwrap()
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        }
    }
}
