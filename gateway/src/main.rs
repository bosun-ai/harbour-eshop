mod config;
mod gateway;
mod legacy;
mod transport;

use std::sync::Arc;

use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{net::TcpListener, sync::watch, task::JoinSet, time::timeout};
use tokio_rustls::TlsAcceptor;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    config::{Config, Limits},
    gateway::Gateway,
    legacy::LegacyUpstream,
    transport::{DeadlineIo, RequestDeadlines},
};

#[tokio::main]
async fn main() {
    if let Err(class) = run().await {
        // Never print configuration, paths, library error text, or TLS material.
        eprintln!("gateway startup/runtime failure: {class}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), &'static str> {
    let path = std::env::var_os("GATEWAY_CONFIG").ok_or("config_required")?;
    let config = Config::load(std::path::Path::new(&path))?;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(false)
                .with_filter(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("eshop_gateway", config.log_level.filter()),
                ),
        )
        .init();
    let tls = TlsAcceptor::from(legacy::public_tls(&config)?);
    let legacy = Arc::new(LegacyUpstream::new(&config)?);
    let (shutdown, receiver) = watch::channel(false);
    // Future slices add one handler and one registration here, never in the transport.
    let gateway = Arc::new(Gateway::new(&config, legacy, vec![], receiver.clone())?);
    let public = TcpListener::bind(config.public_bind)
        .await
        .map_err(|_| "public_bind_failed")?;
    let management = TcpListener::bind(config.management_bind)
        .await
        .map_err(|_| "management_bind_failed")?;
    let mut connections = JoinSet::new();
    let termination = terminate();
    tokio::pin!(termination);
    tracing::info!("started");
    loop {
        tokio::select! {
            _ = &mut termination => break,
            result = public.accept() => {
                let (stream, peer) = result.map_err(|_| "accept_failed")?;
                let tls = tls.clone();
                let gateway = gateway.clone();
                let mut receiver = receiver.clone();
                let limits = config.limits.clone();
                connections.spawn(async move {
                    let stream = tokio::select! {
                        _ = receiver.changed() => return,
                        result = timeout(Limits::duration(limits.connect_ms), tls.accept(stream)) => match result {
                            Ok(Ok(stream)) => stream,
                            _ => { tracing::warn!(error_class = "client_tls", "connection_rejected"); return; }
                        },
                    };
                    let deadlines = RequestDeadlines::new(limits.body_idle_ms);
                    let service_deadlines = deadlines.clone();
                    let service = service_fn(move |request| {
                        let gateway = gateway.clone();
                        let deadlines = service_deadlines.clone();
                        let id = deadlines.start(limits.total_request_ms);
                        let head = request.method() == hyper::Method::HEAD;
                        async move {
                            gateway.public(request, peer).await.map(|response| {
                                response.map(|body| deadlines.response(id, body, head))
                            })
                        }
                    });
                    let mut builder = http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(Limits::duration(limits.client_header_ms));
                    let connection = builder.serve_connection(DeadlineIo {
                        inner: TokioIo::new(stream), deadlines: deadlines.clone(),
                    }, service);
                    tokio::pin!(connection);
                    loop {
                        tokio::select! {
                            result = &mut connection => {
                                if result.is_err() { tracing::warn!(error_class = "client_or_stream", "connection_closed"); }
                                break;
                            },
                            _ = deadlines.expired() => {
                                tracing::warn!(error_class = "request_deadline", "connection_closed");
                                break;
                            },
                            _ = receiver.changed(), if !*receiver.borrow() => {
                                connection.as_mut().graceful_shutdown();
                            }
                        }
                    }
                });
            },
            result = management.accept() => {
                let (stream, _) = result.map_err(|_| "accept_failed")?;
                let gateway = gateway.clone();
                let mut receiver = receiver.clone();
                let header_ms = config.limits.client_header_ms;
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let gateway = gateway.clone();
                        async move { gateway.management(request).await }
                    });
                    let mut builder = http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(Limits::duration(header_ms));
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = receiver.changed() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                    }
                });
            },
            _ = connections.join_next(), if !connections.is_empty() => {},
        }
    }
    drop(public);
    drop(management);
    let _ = shutdown.send(true);
    tracing::info!("draining");
    if timeout(Limits::duration(config.limits.shutdown_ms), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        tracing::warn!(error_class = "shutdown_deadline", "drain_aborted");
    }
    tracing::info!("stopped");
    Ok(())
}

async fn terminate() {
    #[cfg(unix)]
    {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        tokio::select! { _ = signal.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
