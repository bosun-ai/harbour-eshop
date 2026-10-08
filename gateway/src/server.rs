use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use http_body_util::BodyExt;
use hyper::{Request, Response, StatusCode, body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{net::TcpListener, sync::watch, task::JoinSet, time::timeout};
use tokio_rustls::TlsAcceptor;

use crate::{
    config::Config,
    dispatch::{Context, Dispatch},
    legacy::{Body, LegacyUpstream, TransportError, full, timed},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct LoggedBody {
    inner: Body,
    start: Instant,
    id: u64,
    owner: String,
    method: hyper::Method,
    status: u16,
    finished: bool,
}

impl LoggedBody {
    fn finish(&mut self, outcome: &'static str) {
        if !self.finished {
            self.finished = true;
            tracing::info!(correlation_id = self.id, route = self.owner, owner = self.owner,
                method = %self.method, status = self.status,
                duration_ms = self.start.elapsed().as_millis() as u64, outcome);
        }
    }
}

impl HttpBody for LoggedBody {
    type Data = Bytes;
    type Error = TransportError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(context);
        match &result {
            Poll::Ready(Some(Err(_))) => self.finish("body_interrupted"),
            Poll::Ready(None) => self.finish("complete"),
            Poll::Ready(Some(Ok(_))) if self.inner.is_end_stream() => self.finish("complete"),
            _ => {}
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        self.finish(if self.inner.is_end_stream() {
            "complete"
        } else {
            "body_cancelled"
        });
    }
}

fn response(status: StatusCode, text: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(full(text))
        .unwrap()
}

async fn application(
    request: Request<Incoming>,
    peer: std::net::SocketAddr,
    config: Arc<Config>,
    dispatch: Arc<Dispatch>,
) -> Result<Response<Body>, Infallible> {
    let start = Instant::now();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let method = request.method().clone();
    let (owner, handler, allowed) = dispatch.select(request.uri().path(), &method);
    let (parts, body) = request.into_parts();
    let request = Request::from_parts(
        parts,
        timed(
            body.map_err(|_| TransportError::Client).boxed_unsync(),
            config.client_read,
        ),
    );
    let (result, outcome) = if let Some(handler) = handler {
        match handler
            .handle(
                request,
                Context {
                    peer,
                    correlation_id: id,
                },
            )
            .await
        {
            Ok(response) => (response, "headers"),
            Err(TransportError::Timeout) => (
                response(StatusCode::GATEWAY_TIMEOUT, "Gateway timeout\n"),
                "timeout",
            ),
            Err(_) => (
                response(StatusCode::BAD_GATEWAY, "Upstream unavailable\n"),
                "transport_error",
            ),
        }
    } else {
        let mut result = response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed\n");
        result.headers_mut().insert(
            "allow",
            allowed
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join(", ")
                .parse()
                .unwrap(),
        );
        (result, "method_policy")
    };
    // Route label is ownership, never a client-supplied path or query.
    tracing::info!(correlation_id = id, route = owner, owner, method = %method, status = result.status().as_u16(), duration_ms = start.elapsed().as_millis() as u64, outcome);
    let status = result.status().as_u16();
    Ok(result.map(|inner| {
        LoggedBody {
            inner,
            start,
            id,
            owner: owner.to_owned(),
            method,
            status,
            finished: false,
        }
        .boxed_unsync()
    }))
}

pub async fn run(
    config: Arc<Config>,
    dispatch: Arc<Dispatch>,
    legacy: Arc<LegacyUpstream>,
) -> Result<(), String> {
    let public = TcpListener::bind(config.public_bind)
        .await
        .map_err(|_| "PUBLIC_BIND: cannot listen")?;
    let admin = TcpListener::bind(config.admin_bind)
        .await
        .map_err(|_| "ADMIN_BIND: cannot listen")?;
    let acceptor = TlsAcceptor::from(config.public_tls.clone());
    let (shutdown, stopping) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let signal = async {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("signal registration");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    };
    tokio::pin!(signal);
    tracing::info!(event = "listening");
    loop {
        tokio::select! {
            _ = &mut signal => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if result.is_err() { tracing::warn!(event = "connection_task_failed"); }
            }
            accepted = public.accept() => {
                let (stream, peer) = accepted.map_err(|_| "public accept failed")?;
                let (acceptor, config, dispatch, mut stopping) = (acceptor.clone(), config.clone(), dispatch.clone(), stopping.clone());
                tasks.spawn(async move {
                    let stream = tokio::select! {
                        _ = stopping.changed() => return,
                        result = timeout(config.connect, acceptor.accept(stream)) => match result { Ok(Ok(stream)) => stream, _ => return },
                    };
                    let service = service_fn(move |request| application(request, peer, config.clone(), dispatch.clone()));
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(30));
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        result = &mut connection => { if result.is_err() { tracing::warn!(event = "public_connection_interrupted"); } }
                        _ = stopping.changed() => {
                            connection.as_mut().graceful_shutdown();
                            if connection.await.is_err() { tracing::warn!(event = "public_connection_interrupted"); }
                        }
                    }
                });
            }
            accepted = admin.accept() => {
                let (stream, _) = accepted.map_err(|_| "admin accept failed")?;
                let (legacy, mut stopping) = (legacy.clone(), stopping.clone());
                tasks.spawn(async move {
                    let state = stopping.clone();
                    let service = service_fn(move |request: Request<Incoming>| {
                        let (legacy, state) = (legacy.clone(), state.clone());
                        async move {
                            let result = match (request.method().as_str(), request.uri().path()) {
                                ("GET", "/live") => response(StatusCode::OK, "Live\n"),
                                ("GET", "/ready") if !*state.borrow() && legacy.ready().await && !*state.borrow() => response(StatusCode::OK, "Ready\n"),
                                ("GET", "/ready") => response(StatusCode::SERVICE_UNAVAILABLE, "Not ready\n"),
                                _ => response(StatusCode::NOT_FOUND, "Not found\n"),
                            };
                            Ok::<_, Infallible>(result)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(3));
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stopping.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; }
                    }
                });
            }
        }
    }
    shutdown.send_replace(true);
    drop(public);
    drop(admin);
    tracing::info!(event = "draining");
    if timeout(config.shutdown, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!(event = "drain_timeout");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    tracing::info!(event = "stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{Handler, HandlerResult, Path, Registration};
    use hyper::Method;
    use std::{collections::HashSet, net::SocketAddr, sync::Mutex};

    #[derive(Debug)]
    struct Call {
        owner: &'static str,
        method: Method,
        uri: String,
        host: String,
        body: Bytes,
        peer: SocketAddr,
        correlation_id: u64,
    }

    struct RecordingHandler {
        owner: &'static str,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl Handler for RecordingHandler {
        fn handle(&self, request: Request<Body>, context: Context) -> HandlerResult {
            let (owner, calls) = (self.owner, self.calls.clone());
            Box::pin(async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await?.to_bytes();
                calls.lock().unwrap().push(Call {
                    owner,
                    method: parts.method,
                    uri: parts.uri.to_string(),
                    host: parts.headers["host"].to_str().unwrap().to_owned(),
                    body,
                    peer: context.peer,
                    correlation_id: context.correlation_id,
                });
                Ok(Response::builder()
                    .status(StatusCode::CREATED)
                    .header("x-test-owner", owner)
                    .body(full(owner))
                    .unwrap())
            })
        }
    }

    #[tokio::test]
    async fn application_executes_only_the_activated_owner() {
        let config = Arc::new(Config {
            public_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            upstream: "https://localhost/".parse().unwrap(),
            enabled: HashSet::new(),
            connect: Duration::from_secs(1),
            client_read: Duration::from_secs(1),
            upstream_timeout: Duration::from_secs(1),
            shutdown: Duration::from_secs(1),
            log_level: tracing::Level::INFO,
            public_tls: Arc::new(
                rustls::ServerConfig::builder()
                    .with_no_client_auth()
                    .with_cert_resolver(
                        Arc::new(rustls::server::ResolvesServerCertUsingSni::new()),
                    ),
            ),
            upstream_tls: Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth(),
            ),
        });
        let calls = Arc::new(Mutex::new(Vec::<Call>::new()));
        let handler = |owner| -> Arc<dyn Handler> {
            Arc::new(RecordingHandler {
                owner,
                calls: calls.clone(),
            })
        };
        let mut correlation_ids = HashSet::new();
        for (registered, enabled) in [(false, false), (true, false), (true, true)] {
            let registrations = if registered {
                vec![
                    Registration {
                        id: "exact",
                        path: Path::Exact("/test/exact"),
                        methods: vec![Method::GET, Method::POST],
                        handler: handler("exact"),
                    },
                    Registration {
                        id: "family",
                        path: Path::Family("/test/family"),
                        methods: vec![Method::GET, Method::POST],
                        handler: handler("family"),
                    },
                ]
            } else {
                vec![]
            };
            let dispatch = Arc::new(
                Dispatch::new(
                    registrations,
                    if enabled {
                        HashSet::from(["exact".into(), "family".into()])
                    } else {
                        HashSet::new()
                    },
                    handler("legacy"),
                )
                .unwrap(),
            );
            for (path, method, owner) in [
                ("/test/exact?raw=%2F", Method::POST, "exact"),
                ("/test/family", Method::GET, "family"),
                ("/test/family/child?raw=%2B", Method::POST, "family"),
                ("/test/exact/child", Method::POST, "legacy"),
                ("/test/familyish", Method::POST, "legacy"),
                ("/unknown", Method::POST, "legacy"),
                ("/test/exact", Method::PUT, "exact"),
                ("/test/family/child", Method::PUT, "family"),
            ] {
                // HTTP/1 supplies a real Incoming body to the production application path.
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                let peer = stream.local_addr().unwrap();
                let (server_stream, _) = listener.accept().await.unwrap();
                let (config, dispatch) = (config.clone(), dispatch.clone());
                let server = tokio::spawn(async move {
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(server_stream),
                            service_fn(move |request| {
                                application(request, peer, config.clone(), dispatch.clone())
                            }),
                        )
                        .await
                        .unwrap();
                });
                let (mut sender, connection) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream))
                        .await
                        .unwrap();
                let client = tokio::spawn(connection);
                let before = calls.lock().unwrap().len();
                let result = timeout(
                    Duration::from_secs(3),
                    sender.send_request(
                        Request::builder()
                            .method(method.clone())
                            .uri(path)
                            .header("host", "public.example:8002")
                            .header("connection", "close")
                            .body(full("test request body"))
                            .unwrap(),
                    ),
                )
                .await
                .unwrap()
                .unwrap();
                let denied = enabled && owner != "legacy" && method == Method::PUT;
                if denied {
                    assert_eq!(result.status(), StatusCode::METHOD_NOT_ALLOWED);
                    assert_eq!(result.headers()["allow"], "GET, POST");
                    assert_eq!(calls.lock().unwrap().len(), before);
                    assert_eq!(
                        result.into_body().collect().await.unwrap().to_bytes(),
                        "Method not allowed\n"
                    );
                } else {
                    let expected = if enabled { owner } else { "legacy" };
                    assert_eq!(result.status(), StatusCode::CREATED);
                    assert_eq!(result.headers()["x-test-owner"], expected);
                    assert_eq!(
                        result.into_body().collect().await.unwrap().to_bytes(),
                        expected
                    );
                    let calls = calls.lock().unwrap();
                    assert_eq!(calls.len(), before + 1);
                    let call = calls.last().unwrap();
                    assert_eq!(call.owner, expected);
                    assert_eq!(call.method, method);
                    assert_eq!(call.uri, path);
                    assert_eq!(call.host, "public.example:8002");
                    assert_eq!(call.body, "test request body");
                    assert_eq!(call.peer, peer);
                    assert_ne!(call.correlation_id, 0);
                    assert!(correlation_ids.insert(call.correlation_id));
                }
                timeout(Duration::from_secs(3), server)
                    .await
                    .unwrap()
                    .unwrap();
                timeout(Duration::from_secs(3), client)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
        }
        assert!(crate::dispatch::registrations().is_empty());
    }
}
