//! Public HTTPS and private health listeners with bounded connection draining.
use crate::{
    Error,
    config::Config,
    dispatch::Dispatcher,
    legacy::{LegacyUpstream, bounded_body},
    response,
};
use hyper::{Request, StatusCode, body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::{net::TcpListener, sync::watch, task::JoinSet, time::timeout};
use tokio_rustls::TlsAcceptor;
use tracing::Instrument;

pub async fn run(
    config: Config,
    dispatcher: Dispatcher,
    upstream: LegacyUpstream,
) -> Result<(), Error> {
    let public = TcpListener::bind(config.public).await?;
    let admin = TcpListener::bind(config.admin).await?;
    let tls = TlsAcceptor::from(Arc::new(config.tls));
    let dispatcher = Arc::new(dispatcher);
    let counter = Arc::new(AtomicU64::new(1));
    let (shutdown, stopping) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let signal = shutdown_signal();
    tokio::pin!(signal);
    tracing::info!("gateway listening");
    loop {
        tokio::select! {
            _ = &mut signal => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = public.accept() => {
                let (tcp, _) = accepted?;
                let tls = tls.clone();
                let upstream = upstream.clone();
                let dispatcher = dispatcher.clone();
                let counter = counter.clone();
                let mut stopping = stopping.clone();
                let idle = config.idle;
                let handshake = config.connect;
                tasks.spawn(async move {
                    let stream = tokio::select! {
                        _ = stopping.changed() => return,
                        result = timeout(handshake, tls.accept(tcp)) => match result { Ok(Ok(stream)) => stream, _ => return },
                    };
                    let service = service_fn(move |request: Request<Incoming>| {
                        let upstream = upstream.clone();
                        let dispatcher = dispatcher.clone();
                        let request_id = counter.fetch_add(1, Ordering::Relaxed);
                        let span = tracing::info_span!("request", request_id);
                        async move {
                            let started = Instant::now();
                            let method = request.method().clone();
                            let selected = dispatcher.select(&method, request.uri().path());
                            let owner = selected.map(|entry| entry.id).unwrap_or("legacy");
                            let request = request.map(|body| bounded_body(body, idle, None));
                            let result = match selected { Some(entry) => (entry.handler)(request).await, None => upstream.forward(request).await };
                            // Extension methods are untrusted strings, not log labels.
                            let method_label = match method.as_str() {
                                "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "TRACE" | "PATCH" => method.as_str(),
                                _ => "OTHER",
                            };
                            tracing::info!(request_id, method = method_label, owner, status = result.status().as_u16(), duration_ms = started.elapsed().as_millis() as u64, "request headers completed");
                            Ok::<_, Infallible>(result)
                        }.instrument(span)
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(idle);
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stopping.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; }
                    }
                });
            },
            accepted = admin.accept() => {
                let (tcp, _) = accepted?;
                let upstream = upstream.clone();
                let mut stopping = stopping.clone();
                let idle = config.idle;
                tasks.spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let upstream = upstream.clone();
                        async move {
                            let result = match (request.method().as_str(), request.uri().path()) {
                                ("GET", "/live") => response(StatusCode::OK, "live\n"),
                                ("GET", "/ready") if upstream.ready().await => response(StatusCode::OK, "ready\n"),
                                ("GET", "/ready") => response(StatusCode::SERVICE_UNAVAILABLE, "not ready\n"),
                                _ => response(StatusCode::NOT_FOUND, "not found\n"),
                            };
                            Ok::<_, Infallible>(result)
                        }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(idle);
                    let connection = builder.serve_connection(TokioIo::new(tcp), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stopping.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; }
                    }
                });
            }
        }
    }
    drop(public);
    drop(admin);
    let _ = shutdown.send(true);
    if timeout(config.drain, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    tracing::info!("gateway stopped");
    Ok(())
}

async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
}
