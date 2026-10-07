//! Opaque HTTP/1.1 adapter: single submission, no retries, pooling, or URL rebuilding.
use crate::{
    Body,
    config::{ValidatedConfig, milliseconds},
    response,
};
use bytes::Bytes;
use futures_util::stream;
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    HeaderMap, Request, Response,
    body::{Frame, Incoming},
    header::{CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING},
};
use hyper_util::rt::TokioIo;
use std::{net::IpAddr, sync::Arc};
use tokio::{
    net::TcpStream,
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_rustls::TlsConnector;

struct Driver(JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Bound body idle time and total response time. Dropping the body cancels upstream IO.
fn bounded_body(
    body: Incoming,
    idle_ms: u64,
    deadline: Option<Instant>,
    driver: Option<Driver>,
) -> Body {
    StreamBody::new(stream::unfold(
        (body, driver, false),
        move |(mut body, driver, done)| async move {
            if done {
                return None;
            }
            let wait = deadline
                .map(|end| end.saturating_duration_since(Instant::now()))
                .map_or(milliseconds(idle_ms), |remaining| {
                    remaining.min(milliseconds(idle_ms))
                });
            let frame: Result<Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>> =
                match timeout(wait, body.frame()).await {
                    Ok(Some(Ok(frame))) => Ok(frame),
                    Ok(Some(Err(error))) => Err(Box::new(error)),
                    Ok(None) => return None,
                    Err(_) => Err("body deadline exceeded".into()),
                };
            let done = frame.is_err();
            Some((frame, (body, driver, done)))
        },
    ))
    .boxed_unsync()
}

/// Strip connection-specific fields, including fields nominated by Connection.
pub fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(|name| name.trim().to_owned()))
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

pub struct LegacyUpstream {
    pub config: Arc<ValidatedConfig>,
}

impl LegacyUpstream {
    pub fn new(config: Arc<ValidatedConfig>) -> Self {
        Self { config }
    }

    /// Preserve known-length bytes and the raw origin-form target. Reject unknown-length
    /// framing explicitly because hbhttpd cannot decode chunked request bodies.
    pub async fn forward(&self, mut request: Request<Incoming>, peer: IpAddr) -> Response<Body> {
        if request.headers().contains_key(TRANSFER_ENCODING) {
            return response(411, "Content-Length required\n");
        }
        let lengths: Vec<_> = request.headers().get_all(CONTENT_LENGTH).iter().collect();
        if lengths.len() > 1
            || lengths.first().is_some_and(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|text| text.parse::<u64>().ok())
                    .is_none()
            })
        {
            return response(400, "Invalid framing\n");
        }
        if request.uri().scheme().is_some()
            || request.method() == hyper::Method::CONNECT
            || request.headers().contains_key("upgrade")
        {
            return response(400, "Unsupported request target or upgrade\n");
        }
        let host = match request.headers().get(HOST).cloned() {
            Some(host) => host,
            None => return response(400, "Host required\n"),
        };
        // Keep framing and Host even if a malicious Connection field nominates them.
        let length = request.headers().get(CONTENT_LENGTH).cloned();
        let trusted = self
            .config
            .settings
            .trusted_proxy_cidrs
            .iter()
            .any(|cidr| cidr.contains(&peer));
        let prior = if trusted {
            request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .filter(|value| {
                    value
                        .split(',')
                        .all(|ip| ip.trim().parse::<IpAddr>().is_ok())
                })
                .map(str::to_owned)
        } else {
            None
        };
        strip_hop_headers(request.headers_mut());
        let forwarding: Vec<_> = request
            .headers()
            .keys()
            .filter(|name| {
                name.as_str() == "forwarded"
                    || name.as_str().starts_with("x-forwarded-")
                    || name.as_str() == "x-request-id"
            })
            .cloned()
            .collect();
        for name in forwarding {
            request.headers_mut().remove(name);
        }
        request.headers_mut().insert(HOST, host.clone());
        if let Some(length) = length {
            request.headers_mut().insert(CONTENT_LENGTH, length);
        } else {
            request
                .headers_mut()
                .insert(CONTENT_LENGTH, "0".parse().unwrap());
        }
        request.headers_mut().insert("x-forwarded-host", host);
        request
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
        let forwarded_for =
            prior.map_or_else(|| peer.to_string(), |prior| format!("{prior}, {peer}"));
        request
            .headers_mut()
            .insert("x-forwarded-for", forwarded_for.parse().unwrap());
        *request.version_mut() = hyper::Version::HTTP_11;
        let (parts, body) = request.into_parts();
        let request = Request::from_parts(
            parts,
            bounded_body(body, self.config.settings.body_idle_timeout_ms, None, None),
        );
        self.submit(request).await
    }

    async fn submit(&self, request: Request<Body>) -> Response<Body> {
        let config = &self.config;
        let connection = timeout(milliseconds(config.settings.connect_timeout_ms), async {
            let tcp =
                TcpStream::connect((config.upstream_host.as_str(), config.upstream_port)).await?;
            let name = rustls::pki_types::ServerName::try_from(config.upstream_host.clone())?;
            let tls = TlsConnector::from(config.client_tls.clone())
                .connect(name, tcp)
                .await?;
            let (sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((sender, connection))
        })
        .await;
        let (mut sender, connection) = match connection {
            Ok(Ok(connection)) => connection,
            Ok(Err(_)) => return response(502, "Upstream connection failed\n"),
            Err(_) => return response(504, "Upstream connection timed out\n"),
        };
        let driver = Driver(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let deadline = Instant::now() + milliseconds(config.settings.response_timeout_ms);
        match tokio::time::timeout_at(deadline, sender.send_request(request)).await {
            Ok(Ok(mut result)) => {
                strip_hop_headers(result.headers_mut());
                let (parts, body) = result.into_parts();
                Response::from_parts(
                    parts,
                    bounded_body(
                        body,
                        config.settings.body_idle_timeout_ms,
                        Some(deadline),
                        Some(driver),
                    ),
                )
            }
            Ok(Err(_)) => response(502, "Upstream exchange failed; outcome unknown\n"),
            Err(_) => response(504, "Upstream response timed out; outcome unknown\n"),
        }
    }

    /// Probe authenticated TLS and a small, bounded legacy /hello response.
    pub async fn ready(&self) -> bool {
        let request = Request::builder()
            .uri("/hello")
            .header(HOST, &self.config.upstream_host)
            .body(crate::response(200, "").into_body())
            .unwrap();
        let mut result = self.submit(request).await;
        if result.status() != 200 {
            return false;
        }
        let mut bytes = Vec::new();
        while let Some(frame) = result.body_mut().frame().await {
            match frame {
                Ok(frame) => {
                    if let Ok(data) = frame.into_data() {
                        if bytes.len() + data.len() > 1024 {
                            return false;
                        }
                        bytes.extend_from_slice(&data);
                    }
                }
                Err(_) => return false,
            }
        }
        bytes.windows(6).any(|window| window == b"Hello!")
    }
}
