use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use harbour_eshop_gateway::{
    config::GatewayConfig,
    dispatch::{bootstrap_registrations, Dispatch, Dispatcher},
    proxy::LegacyUpstream,
    tls,
};
use hyper::{body::Incoming, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinSet};
use tokio_rustls::TlsAcceptor;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = GatewayConfig::from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(&config.log_level)
        .without_time()
        .init();
    let dispatcher = Arc::new(Dispatcher::new(
        bootstrap_registrations(),
        &config.enabled_slices,
    )?);
    let upstream = Arc::new(LegacyUpstream::new(
        config.legacy_url.clone(),
        tls::client_config(&config.upstream_ca)?,
        config.connect_timeout,
        config.request_timeout,
        config.response_idle_timeout,
    ));
    let listener = TcpListener::bind(config.bind_address).await?;
    let acceptor = TlsAcceptor::from(Arc::new(tls::server_config(
        &config.public_certificate,
        &config.public_key,
    )?));
    tracing::info!(address = %config.bind_address, "gateway listening");
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            signal = shutdown_signal() => { signal?; tracing::info!("shutdown signal received; draining connections"); break; }
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let acceptor = acceptor.clone(); let dispatcher = Arc::clone(&dispatcher); let upstream = Arc::clone(&upstream);
                connections.spawn(async move {
                    let stream = match acceptor.accept(stream).await { Ok(stream) => stream, Err(error) => { tracing::warn!(error = %error, "public TLS handshake failed"); return; } };
                    let service = service_fn(move |request: Request<Incoming>| route(request, peer, Arc::clone(&dispatcher), Arc::clone(&upstream)));
                    if let Err(error) = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await { tracing::debug!(error = %error, "client connection closed with error"); }
                });
            }
        }
    }
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn route(
    request: Request<Incoming>,
    peer: SocketAddr,
    dispatcher: Arc<Dispatcher>,
    upstream: Arc<LegacyUpstream>,
) -> Result<Response<harbour_eshop_gateway::proxy::GatewayBody>, Infallible> {
    Ok(match dispatcher.dispatch(&request) {
        Dispatch::Slice(handler) => handler.handle(request).await,
        Dispatch::LegacyUpstream => upstream.forward(request, peer.ip()).await,
    })
}
async fn shutdown_signal() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            signal = tokio::signal::ctrl_c() => signal,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}
