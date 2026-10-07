//! Hosting-only HTTPS ingress; all application ownership stays with Harbour.
pub mod config;
pub mod dispatch;
pub mod legacy;
pub mod operations;
pub mod server;
pub mod slices;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::Response;

/// Streaming body shared by integration adapters and future domain handlers.
pub type Body = UnsyncBoxBody<Bytes, dispatch::TransportFailure>;

/// Construct a small gateway-owned diagnostic response, never a legacy page.
pub fn response(status: u16, text: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(
            Full::new(Bytes::from_static(text.as_bytes()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("static response")
}

/// Validate all configuration and activation before opening either listener.
pub async fn run(path: &std::ffi::OsStr) -> Result<(), &'static str> {
    let config = config::Config::load(std::path::Path::new(path))?;
    let upstream = std::sync::Arc::new(legacy::LegacyUpstream::new(&config)?);
    let registry = dispatch::DispatchRegistry::new(
        upstream.clone(),
        slices::register(),
        &config.enabled_families,
    )?;
    server::run(config, upstream, std::sync::Arc::new(registry)).await
}
