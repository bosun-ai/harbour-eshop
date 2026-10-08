use crate::{
    Error,
    config::{Config, setting},
    gateway::{self, Context, Registration},
    legacy::{self, Legacy},
};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    fs::File,
    io::BufReader,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig};

// Bound each output phase through successful flush, including buffered bodies.
pub(crate) struct WriteDeadline<T> {
    io: T,
    limit: Duration,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<T> WriteDeadline<T> {
    pub(crate) fn new(io: T, limit: Duration) -> Self {
        Self {
            io,
            limit,
            timer: None,
        }
    }

    fn check(&mut self, context: &mut TaskContext<'_>) -> std::io::Result<()> {
        use std::future::Future;
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.limit)));
        if timer.as_mut().poll(context).is_ready() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "downstream write deadline exceeded",
            ));
        }
        Ok(())
    }
}

impl<T: hyper::rt::Read + Unpin> hyper::rt::Read for WriteDeadline<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(context, buffer)
    }
}

impl<T: hyper::rt::Write + Unpin> hyper::rt::Write for WriteDeadline<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.check(context)?;
        Pin::new(&mut self.io).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        self.check(context)?;
        Pin::new(&mut self.io).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.check(context)?;
        let result = Pin::new(&mut self.io).poll_flush(context);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.timer = None;
        }
        result
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.check(context)?;
        Pin::new(&mut self.io).poll_shutdown(context)
    }
}

pub(crate) async fn healthcheck() -> Result<(), Error> {
    let address = setting("ADMIN_BIND", "127.0.0.1:8003");
    timeout(Duration::from_secs(10), async {
        let tcp = TcpStream::connect(address).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
        let driver = tokio::spawn(connection);
        let result = sender
            .send_request(
                Request::builder()
                    .uri("/ready")
                    .header("host", "localhost")
                    .body(Full::new(Bytes::new()))?,
            )
            .await;
        driver.abort();
        if result?.status() != StatusCode::OK {
            return Err("not ready".into());
        }
        Ok::<_, Error>(())
    })
    .await?
}

pub(crate) async fn serve(config: Config, entries: Vec<Registration>) -> Result<(), Error> {
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(&config.key)?))?
        .ok_or("missing private key")?;
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(legacy::certificates(&config.cert)?, key)?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let legacy = Legacy::new(&config)?;
    let public = TcpListener::bind(config.public_bind).await?;
    let admin = TcpListener::bind(config.admin_bind).await?;
    let config = Arc::new(config);
    let entries = Arc::new(entries);
    let sequence = Arc::new(AtomicU64::new(1));
    let (stop, stopping) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = public.accept() => {
                let (tcp, peer) = accepted?;
                let acceptor = acceptor.clone();
                let legacy = legacy.clone();
                let config = config.clone();
                let entries = entries.clone();
                let sequence = sequence.clone();
                let mut stopping = stopping.clone();
                tasks.spawn(async move {
                    let Ok(Ok(tls)) = timeout(config.connect, acceptor.accept(tcp)).await else { return; };
                    let header_timeout = config.upload;
                    let write_timeout = config.response;
                    let service = service_fn(move |mut request: Request<hyper::body::Incoming>| {
                        let legacy = legacy.clone();
                        let config = config.clone();
                        let entries = entries.clone();
                        let request_id = sequence.fetch_add(1, Ordering::Relaxed);
                        async move {
                            let started = Instant::now();
                            let method = request.method().clone();
                            let chunked = request.headers().contains_key("transfer-encoding");
                            let expect = request.headers().contains_key("expect");
                            let nominated_framing = request.headers().get_all("connection").iter().filter_map(|value| value.to_str().ok()).flat_map(|value| value.split(',')).any(|value| value.trim().eq_ignore_ascii_case("content-length"));
                            legacy::strip_untrusted(request.headers_mut());
                            let owner = gateway::select(&entries, &config.enabled, request.uri().path(), &method);
                            let owner_id = owner.map_or("legacy", |entry| entry.id);
                            let response = if nominated_framing { legacy::text(StatusCode::BAD_REQUEST, "Invalid framing nomination\n") }
                            else if chunked { legacy::text(StatusCode::LENGTH_REQUIRED, "Content-Length required\n") }
                            else if expect { legacy::text(StatusCode::EXPECTATION_FAILED, "Expect unsupported\n") }
                            else {
                                let (parts, body) = request.into_parts();
                                let request = Request::from_parts(parts, legacy::incoming(body, Instant::now() + config.upload));
                                match owner {
                                    Some(entry) => match timeout(config.response, (entry.handler)(request, Context { request_id, peer })).await {
                                        Ok(Ok(response)) => response,
                                        Ok(Err(_)) => legacy::text(StatusCode::INTERNAL_SERVER_ERROR, "Handler failed\n"),
                                        Err(_) => legacy::text(StatusCode::GATEWAY_TIMEOUT, "Handler deadline\n"),
                                    },
                                    None => match legacy.forward(request).await {
                                        Ok(response) => response,
                                        Err(status) => legacy::text(status, "Upstream unavailable\n"),
                                    },
                                }
                            };
                            tracing::info!(method = %method, owner = owner_id, status = response.status().as_u16(), latency_ms = started.elapsed().as_millis() as u64, request_id);
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(header_timeout).auto_date_header(false);
                    let connection = builder.serve_connection(WriteDeadline::new(TokioIo::new(tls), write_timeout), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stopping.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; }
                    }
                });
            },
            accepted = admin.accept() => {
                let (tcp, _) = accepted?;
                let legacy = legacy.clone();
                let limit = config.response;
                tasks.spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let legacy = legacy.clone();
                        async move {
                            let status = match (request.method().as_str(), request.uri().path()) {
                                ("GET", "/live") => StatusCode::OK,
                                ("GET", "/ready") if legacy.ready().await => StatusCode::OK,
                                ("GET", "/ready") => StatusCode::SERVICE_UNAVAILABLE,
                                _ => StatusCode::NOT_FOUND,
                            };
                            Ok::<_, Infallible>(legacy::text(status, "health\n"))
                        }
                    });
                    let _ = timeout(limit, hyper::server::conn::http1::Builder::new().keep_alive(false).serve_connection(TokioIo::new(tcp), service)).await;
                });
            }
        }
    }
    drop(public);
    drop(admin);
    let _ = stop.send(true);
    if timeout(config.drain, async {
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
