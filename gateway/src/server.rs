//! Bounded TLS serving, exchange deadlines, and gateway-only completion logs.
use crate::{
    Body, Error, body,
    config::Config,
    dispatch::{Context, Dispatch},
};
use http_body_util::BodyExt;
use hyper::{
    Request, Response, StatusCode,
    body::{Body as HttpBody, Frame, SizeHint},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    fmt,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::Instant,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Instant as TokioInstant, Sleep},
};
use tokio_rustls::TlsAcceptor;

/// Classified deadline failure; internal transport details stay private.
#[derive(Debug)]
pub struct Deadline;
impl fmt::Display for Deadline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("deadline")
    }
}
impl std::error::Error for Deadline {}

struct CompletionBody {
    inner: Body,
    timer: Pin<Box<Sleep>>,
    context: Context,
    method: hyper::Method,
    status: u16,
    done: bool,
    _request: Arc<std::sync::Mutex<Body>>,
}

struct SharedRequest(Arc<std::sync::Mutex<Body>>);
impl HttpBody for SharedRequest {
    type Data = bytes::Bytes;
    type Error = Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        task: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Error>>> {
        Pin::new(&mut *self.0.lock().expect("request body lock")).poll_frame(task)
    }
    fn size_hint(&self) -> SizeHint {
        self.0.lock().expect("request body lock").size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.0.lock().expect("request body lock").is_end_stream()
    }
}

impl CompletionBody {
    fn finish(&mut self, category: &'static str) {
        if !self.done {
            self.done = true;
            tracing::info!(owner = self.context.owner, correlation = self.context.correlation,
                method = %self.method, status = self.status,
                duration_ms = self.context.started.elapsed().as_millis() as u64,
                error_category = category, "request complete");
        }
    }
}
impl HttpBody for CompletionBody {
    type Data = bytes::Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        task: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.timer.as_mut().poll(task).is_ready() {
            self.finish("deadline_body");
            return Poll::Ready(Some(Err(Box::new(Deadline))));
        }
        let result = Pin::new(&mut self.inner).poll_frame(task);
        match &result {
            Poll::Ready(None) => self.finish("none"),
            Poll::Ready(Some(Err(_))) => self.finish("upstream_body"),
            Poll::Ready(Some(Ok(_))) if self.inner.is_end_stream() => self.finish("none"),
            _ => {}
        }
        result
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}
impl Drop for CompletionBody {
    fn drop(&mut self) {
        self.finish(if self.inner.is_end_stream() {
            "none"
        } else {
            "cancelled"
        });
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Serve until SIGINT/SIGTERM; bound handshake, connection count, and drain.
pub async fn serve(config: Arc<Config>, dispatch: Arc<Dispatch>) -> Result<(), Error> {
    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|_| "listener bind failed")?;
    let acceptor = TlsAcceptor::from(config.public_tls.clone());
    let limit = Arc::new(Semaphore::new(256));
    let sequence = Arc::new(AtomicU64::new(1));
    let (stop, stopped) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let signal = shutdown();
    tokio::pin!(signal);
    tracing::info!("listener started");
    loop {
        tokio::select! {
            _ = &mut signal => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let (tcp, peer) = accepted.map_err(|_| "listener accept failed")?;
                let Ok(permit) = limit.clone().try_acquire_owned() else { continue; };
                let acceptor = acceptor.clone();
                let config = config.clone();
                let dispatch = dispatch.clone();
                let sequence = sequence.clone();
                let mut stopped = stopped.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let Ok(Ok(tls)) = tokio::time::timeout(config.connect, acceptor.accept(tcp)).await else {
                        tracing::warn!(error_category = "public_tls", "connection rejected");
                        return;
                    };
                    let config_lifetime = config.exchange + config.connect;
                    let retained_upload = Arc::new(std::sync::Mutex::new(None));
                    let service_upload = retained_upload.clone();
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let dispatch = dispatch.clone();
                        let config = config.clone();
                        let correlation = sequence.fetch_add(1, Ordering::Relaxed);
                        let retained_upload = service_upload.clone();
                        async move {
                            let started = Instant::now();
                            let deadline = TokioInstant::now() + config.exchange;
                            let (owner, handler) = dispatch.select(request.uri().path(), request.method());
                            let context = Context { correlation, peer, started, owner };
                            let method = request.method().clone();
                            let (request_parts, incoming) = request.into_parts();
                            // Retain an incomplete upload until the response is sent. Dropping
                            // Incoming early makes Hyper abort even a pre-header 504 response.
                            let upload = Arc::new(std::sync::Mutex::new(incoming.map_err(|error| -> Error { Box::new(error) }).boxed_unsync()));
                            *retained_upload.lock().expect("retained upload lock") = Some(upload.clone());
                            let request = Request::from_parts(request_parts, SharedRequest(upload.clone()).boxed_unsync());
                            let result = tokio::time::timeout_at(deadline, handler.handle(request, context.clone())).await;
                            let (response, category) = match result {
                                Ok(Ok(response)) => (response, "none"),
                                Err(_) => (Response::builder().status(StatusCode::GATEWAY_TIMEOUT).body(body("Gateway timeout")).unwrap(), "deadline"),
                                Ok(Err(error)) => {
                                    let timed_out = error.downcast_ref::<Deadline>().is_some();
                                    (Response::builder().status(if timed_out { StatusCode::GATEWAY_TIMEOUT } else { StatusCode::BAD_GATEWAY })
                                        .body(body("Upstream unavailable")).unwrap(), if timed_out { "deadline" } else { "upstream" })
                                }
                            };
                            if category != "none" {
                                tracing::warn!(correlation, error_category = category, "exchange failed");
                            }
                            let (mut parts, inner) = response.into_parts();
                            if category != "none" {
                                parts.headers.insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("close"));
                            }
                            let deadline = if category == "none" {
                                deadline
                            } else {
                                TokioInstant::now() + config.connect
                            };
                            let stream = CompletionBody { inner, timer: Box::pin(tokio::time::sleep_until(deadline)), context,
                                method, status: parts.status.as_u16(), done: false, _request: upload };
                            Ok::<_, Infallible>(Response::from_parts(parts, stream.boxed_unsync()))
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(true).max_buf_size(64 * 1024);
                    let lifetime = config_lifetime;
                    let connection = builder.serve_connection(TokioIo::new(tls), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = tokio::time::sleep(lifetime) => {
                            tracing::warn!(error_category = "connection_deadline", "connection bounded");
                        },
                        _ = stopped.changed() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                    }
                });
            }
        }
    }
    drop(listener);
    let _ = stop.send(true);
    if tokio::time::timeout(config.drain, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        tracing::warn!(error_category = "drain_timeout", "drain bounded");
    }
    Ok(())
}
