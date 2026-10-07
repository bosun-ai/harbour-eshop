//! Private probes and redacted structured diagnostics.
use crate::{
    Body,
    config::{Config, duration},
    dispatch::{HttpHandler, RequestContext},
    legacy::LegacyUpstream,
};

/// Stream failures may occur after the access record and response headers.
pub fn stream_failure(id: &str, error: crate::dispatch::TransportFailure) {
    println!(
        "{}",
        serde_json::json!({"event": "stream_failure", "correlation_id": id, "transport_error": error.to_string()})
    );
}
use http_body_util::BodyExt;
use hyper::{Request, Response, body::Incoming};
use std::{convert::Infallible, sync::Arc};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

/// Emit only safe metadata, not paths, queries, headers, cookies or credentials.
pub fn log(
    id: &str,
    method: &str,
    owner: &str,
    status: u16,
    started: Instant,
    error: Option<&str>,
) {
    println!(
        "{}",
        serde_json::json!({"correlation_id": id, "method": method, "owner": owner,
        "status": status, "duration_ms": started.elapsed().as_millis(), "transport_error": error})
    );
}

/// Probe reachability with verified TLS and exact Hello! content, not DB health.
pub async fn management(
    request: Request<Incoming>,
    upstream: Arc<LegacyUpstream>,
    config: Arc<Config>,
) -> Result<Response<Body>, Infallible> {
    if request.method() != hyper::Method::GET {
        return Ok(crate::response(404, "not found\n"));
    }
    match request.uri().path() {
        "/livez" => Ok(crate::response(200, "live\n")),
        "/readyz" => {
            let probe = async {
                let context = RequestContext {
                    correlation_id: "readiness".into(),
                    client_ip: config.management_bind.ip(),
                    deadline: Instant::now() + duration(config.probe_ms),
                    cancellation: CancellationToken::new(),
                };
                let request = Request::builder()
                    .uri("/hello")
                    .header("host", "legacy")
                    .header("content-length", "0")
                    .body(crate::response(200, "").into_body())
                    .expect("probe request");
                let response = upstream.handle(request, context).await.ok()?;
                if response.status() != 200 {
                    return None;
                }
                let mut body = response.into_body();
                let mut bytes = Vec::new();
                while let Some(frame) = body.frame().await {
                    let frame = frame.ok()?;
                    if let Some(data) = frame.data_ref() {
                        if bytes.len() + data.len() > 6 {
                            return None;
                        }
                        bytes.extend_from_slice(data);
                    }
                }
                (bytes == b"Hello!").then_some(())
            };
            let ready = matches!(
                timeout(duration(config.probe_ms), probe).await,
                Ok(Some(()))
            );
            Ok(if ready {
                crate::response(200, "ready\n")
            } else {
                crate::response(503, "not ready\n")
            })
        }
        _ => Ok(crate::response(404, "not found\n")),
    }
}
