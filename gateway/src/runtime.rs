//! Listener lifecycle, bounded concurrency, management probes, and secret-free logs.
use crate::{
    config::{ValidatedConfig, milliseconds},
    dispatch::DispatchTable,
    legacy::LegacyUpstream,
    response, slices,
};
use http_body_util::BodyExt;
use hyper::{Request, body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_rustls::TlsAcceptor;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Run HTTPS ingress and a separate HTTP management listener until SIGTERM/SIGINT.
/// At most 128 public connections and 8 management connections are admitted.
pub async fn run(config: ValidatedConfig) -> Result<(), &'static str> {
    let dispatch = Arc::new(DispatchTable::build(
        slices::registry(),
        &config.settings.active_families,
    )?);
    let public = TcpListener::bind(config.settings.public_bind)
        .await
        .map_err(|_| "public bind failed")?;
    let management = TcpListener::bind(config.settings.management_bind)
        .await
        .map_err(|_| "management bind failed")?;
    let config = Arc::new(config);
    let upstream = Arc::new(LegacyUpstream::new(config.clone()));
    let acceptor = TlsAcceptor::from(config.server_tls.clone());
    let public_slots = Arc::new(Semaphore::new(128));
    let management_slots = Arc::new(Semaphore::new(8));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "signal setup failed")?;
    tracing::info!(event = "started");
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = public.accept() => {
                let (socket, peer) = accepted.map_err(|_| "public accept failed")?;
                let Ok(permit) = public_slots.clone().try_acquire_owned() else { continue; };
                let (acceptor, upstream, dispatch, config, mut shutdown) = (acceptor.clone(), upstream.clone(), dispatch.clone(), config.clone(), shutdown_rx.clone());
                tasks.spawn(async move {
                    let _permit = permit;
                    let tls = match timeout(milliseconds(config.settings.connect_timeout_ms), acceptor.accept(socket)).await {
                        Ok(Ok(tls)) => tls, _ => return,
                    };
                    let service = service_fn(move |request: Request<Incoming>| {
                        let (upstream, dispatch) = (upstream.clone(), dispatch.clone());
                        let id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
                        async move {
                            let start = Instant::now();
                            // Only known HTTP tokens are logged; arbitrary extension methods are classified.
                            let method = match request.method().as_str() {
                                "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS" | "CONNECT" | "TRACE" => request.method().as_str().to_owned(),
                                _ => "OTHER".to_owned(),
                            };
                            let selection = dispatch.select(request.method(), request.uri().path());
                            let (route, result) = if let Some((family, handler)) = selection {
                                (family.to_owned(), handler.handle(request.map(|body| body.map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>).boxed_unsync())).await)
                            } else { ("legacy".to_owned(), upstream.forward(request, peer.ip()).await) };
                            tracing::info!(request_id = id, method, route, status = result.status().as_u16(), duration_ms = start.elapsed().as_millis() as u64);
                            Ok::<_, Infallible>(result)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(milliseconds(config.settings.header_timeout_ms))
                        .max_buf_size(32768).keep_alive(false);
                    let connection = builder.serve_connection(TokioIo::new(tls), service);
                    tokio::pin!(connection);
                    tokio::select! { _ = &mut connection => {}, _ = shutdown.changed() => {
                        connection.as_mut().graceful_shutdown(); let _ = connection.await;
                    }}
                });
            },
            accepted = management.accept() => {
                let (socket, _) = accepted.map_err(|_| "management accept failed")?;
                let Ok(permit) = management_slots.clone().try_acquire_owned() else { continue; };
                let (upstream, config, mut shutdown) = (upstream.clone(), config.clone(), shutdown_rx.clone());
                tasks.spawn(async move {
                    let _permit = permit;
                    let service = service_fn(move |request: Request<Incoming>| {
                        let upstream = upstream.clone();
                        async move {
                            let result = match (request.method().as_str(), request.uri().path()) {
                                ("GET", "/live") => response(200, "Live\n"),
                                ("GET", "/ready") if upstream.ready().await => response(200, "Ready\n"),
                                ("GET", "/ready") => response(503, "Not ready\n"),
                                _ => response(404, "Not found\n"),
                            };
                            Ok::<_, Infallible>(result)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(milliseconds(config.settings.header_timeout_ms)).keep_alive(false).max_buf_size(8192);
                    let connection = builder.serve_connection(TokioIo::new(socket), service);
                    tokio::pin!(connection);
                    tokio::select! { _ = &mut connection => {}, _ = shutdown.changed() => {
                        connection.as_mut().graceful_shutdown(); let _ = connection.await;
                    }}
                });
            },
        }
    }
    drop(public);
    drop(management);
    let _ = shutdown_tx.send(true);
    if timeout(milliseconds(config.settings.shutdown_drain_ms), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    tracing::info!(event = "stopped");
    Ok(())
}
