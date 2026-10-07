use crate::proxy::{Body, LegacyUpstream, text};
use hyper::{Request, Response, StatusCode, body::Incoming};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub fn request_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub async fn handle(
    request: Request<Incoming>,
    legacy: LegacyUpstream,
) -> Result<Response<Body>, std::convert::Infallible> {
    Ok(match (request.method().as_str(), request.uri().path()) {
        ("GET", "/live") => text(StatusCode::OK, "live\n"),
        ("GET", "/ready") if legacy.check_legacy().await => text(StatusCode::OK, "ready\n"),
        ("GET", "/ready") => text(StatusCode::SERVICE_UNAVAILABLE, "unavailable\n"),
        _ => text(StatusCode::NOT_FOUND, "not found\n"),
    })
}

pub async fn check_ready() -> Result<(), crate::config::ConfigError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let address = std::env::var("GW_OPS_BIND").unwrap_or_else(|_| "127.0.0.1:9000".into());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(address).await?;
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut bytes = [0; 128];
        let count = stream.read(&mut bytes).await?;
        if !bytes[..count].starts_with(b"HTTP/1.1 200 ") {
            return Err("not ready".into());
        }
        Ok::<_, crate::config::ConfigError>(())
    })
    .await
    .map_err(|_| "readiness timed out")?
}
