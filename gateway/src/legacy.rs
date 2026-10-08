use std::{
    future::Future,
    io::BufReader,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{HeaderMap, Request, Uri, header};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustls::{RootCertStore, ServerConfig};
use tokio::time::{Instant, Sleep, sleep_until, timeout};
use tower::Service;

use crate::{
    config::{Config, Limits},
    gateway::{GatewayFailure, GatewayHandler, HandlerFuture, RequestContext},
};

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type GatewayBody = UnsyncBoxBody<Bytes, BoxError>;
type UpstreamClient = Client<DeadlineConnector, GatewayBody>;

#[derive(Clone)]
struct DeadlineConnector {
    inner: hyper_rustls::HttpsConnector<HttpConnector>,
    budget: std::time::Duration,
}

impl Service<Uri> for DeadlineConnector {
    type Response = <hyper_rustls::HttpsConnector<HttpConnector> as Service<Uri>>::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let future = self.inner.call(uri);
        let budget = self.budget;
        Box::pin(async move {
            timeout(budget, future)
                .await
                .map_err(|_| BoxError::from("connect_deadline"))?
        })
    }
}

pub(crate) fn full(body: impl Into<Bytes>) -> GatewayBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

pub(crate) fn public_tls(config: &Config) -> Result<Arc<ServerConfig>, &'static str> {
    let certificates = certificates(&config.public_certificate)?;
    let file = std::fs::File::open(&config.public_key).map_err(|_| "tls_key_unreadable")?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(|_| "tls_key_invalid")?
        .ok_or("tls_key_invalid")?;
    let mut tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|_| "tls_identity_invalid")?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(tls))
}

fn certificates(
    path: &std::path::Path,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "tls_certificate_unreadable")?;
    let certificates: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<_, _>>()
        .map_err(|_| "tls_certificate_invalid")?;
    if certificates.is_empty() {
        return Err("tls_certificate_invalid");
    }
    Ok(certificates)
}

pub(crate) struct LegacyUpstream {
    client: UpstreamClient,
    upstream: Uri,
    limits: Limits,
}

impl LegacyUpstream {
    pub fn new(config: &Config) -> Result<Self, &'static str> {
        let mut roots = RootCertStore::empty();
        for certificate in certificates(&config.upstream_ca)? {
            roots.add(certificate).map_err(|_| "tls_ca_invalid")?;
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(Limits::duration(config.limits.connect_ms)));
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .retry_canceled_requests(false)
            .build(DeadlineConnector {
                inner: connector,
                budget: Limits::duration(config.limits.connect_ms),
            });
        Ok(Self {
            client,
            upstream: config.validate()?,
            limits: config.limits.clone(),
        })
    }

    pub async fn ready(&self) -> bool {
        let context = RequestContext::probe(self.limits.readiness_ms);
        let request = Request::builder()
            .uri("/hello")
            .body(full(""))
            .expect("static request");
        timeout(Limits::duration(self.limits.readiness_ms), async {
            let response = self.handle(request, context).await.ok()?;
            if response.status() != 200 {
                return None;
            }
            // Bound readiness memory even if the upstream is misconfigured.
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.ok()?;
                if let Ok(data) = frame.into_data() {
                    if bytes.len() + data.len() > 64 {
                        return None;
                    }
                    bytes.extend_from_slice(&data);
                }
            }
            (bytes == b"Hello!").then_some(())
        })
        .await
        .is_ok_and(|result| result.is_some())
    }
}

impl GatewayHandler for LegacyUpstream {
    fn handle(
        &self,
        mut request: Request<GatewayBody>,
        context: RequestContext,
    ) -> HandlerFuture<'_> {
        Box::pin(async move {
            validate_framing(request.headers())?;
            strip_hop_headers(request.headers_mut())?;
            let path = request
                .uri()
                .path_and_query()
                .map(|value| value.as_str())
                .unwrap_or("/");
            let uri = Uri::builder()
                .scheme("https")
                .authority(
                    self.upstream
                        .authority()
                        .expect("validated authority")
                        .clone(),
                )
                .path_and_query(path)
                .build()
                .map_err(|_| GatewayFailure::BadRequest)?;
            *request.uri_mut() = uri;
            *request.version_mut() = hyper::Version::HTTP_11;
            let remaining = context.deadline.saturating_duration_since(Instant::now());
            let budget = remaining.min(Limits::duration(self.limits.upstream_response_ms));
            let mut response = timeout(budget, self.client.request(request))
                .await
                .map_err(|_| GatewayFailure::Deadline)?
                .map_err(|_| GatewayFailure::Upstream)?;
            strip_hop_headers(response.headers_mut()).map_err(|_| GatewayFailure::Upstream)?;
            Ok(response.map(|body| {
                timed_body(
                    body.map_err(|error| Box::new(error) as BoxError)
                        .boxed_unsync(),
                    self.limits.body_idle_ms,
                    context.deadline,
                )
            }))
        })
    }
}

pub(crate) fn validate_framing(headers: &HeaderMap) -> Result<(), GatewayFailure> {
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return Err(if headers.contains_key(header::CONTENT_LENGTH) {
            GatewayFailure::BadRequest
        } else {
            GatewayFailure::LengthRequired
        });
    }
    let lengths: Vec<_> = headers.get_all(header::CONTENT_LENGTH).iter().collect();
    if lengths.len() > 1
        || lengths.first().is_some_and(|value| {
            value.to_str().ok().is_none_or(|value| {
                value.is_empty()
                    || !value.bytes().all(|byte| byte.is_ascii_digit())
                    || value.parse::<u64>().is_err()
            })
        })
    {
        return Err(GatewayFailure::BadRequest);
    }
    Ok(())
}

pub(crate) fn strip_hop_headers(headers: &mut HeaderMap) -> Result<(), GatewayFailure> {
    let mut named = Vec::new();
    for value in headers.get_all(header::CONNECTION) {
        for name in value
            .to_str()
            .map_err(|_| GatewayFailure::BadRequest)?
            .split(',')
        {
            let name = header::HeaderName::from_bytes(name.trim().as_bytes())
                .map_err(|_| GatewayFailure::BadRequest)?;
            if name == header::CONTENT_LENGTH
                || name == header::HOST
                || name == header::TRANSFER_ENCODING
            {
                return Err(GatewayFailure::BadRequest);
            }
            named.push(name);
        }
    }
    for name in named {
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
        "proxy-connection",
    ] {
        headers.remove(name);
    }
    Ok(())
}

// Preserve the source size hint: losing it can turn a known-length form into chunked HTTP/1.1.
pub(crate) fn timed_body(body: GatewayBody, idle_ms: u64, deadline: Instant) -> GatewayBody {
    TimedBody {
        body,
        idle_ms,
        deadline,
        timer: Box::pin(sleep_until(
            deadline.min(Instant::now() + Limits::duration(idle_ms)),
        )),
    }
    .boxed_unsync()
}

struct TimedBody {
    body: GatewayBody,
    idle_ms: u64,
    deadline: Instant,
    timer: Pin<Box<Sleep>>,
}

impl Body for TimedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.body.is_end_stream() {
            return Poll::Ready(None);
        }
        if Instant::now() >= self.deadline || self.timer.as_mut().poll(context).is_ready() {
            return Poll::Ready(Some(Err("body_deadline".into())));
        }
        let result = Pin::new(&mut self.body).poll_frame(context);
        if matches!(result, Poll::Ready(Some(Ok(_)))) {
            let next = self
                .deadline
                .min(Instant::now() + Limits::duration(self.idle_ms));
            self.timer.as_mut().reset(next);
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_and_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", "4".parse().unwrap());
        assert!(validate_framing(&headers).is_ok());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        assert!(validate_framing(&headers).is_err());
        headers.remove("content-length");
        assert!(matches!(
            validate_framing(&headers),
            Err(GatewayFailure::LengthRequired)
        ));
        headers.remove("transfer-encoding");
        headers.insert("connection", "x-secret, keep-alive".parse().unwrap());
        headers.insert("x-secret", "secret".parse().unwrap());
        headers.append("set-cookie", "a=1".parse().unwrap());
        headers.append("set-cookie", "b=2".parse().unwrap());
        strip_hop_headers(&mut headers).unwrap();
        assert!(!headers.contains_key("x-secret"));
        assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
        headers.insert("connection", "content-length".parse().unwrap());
        assert!(strip_hop_headers(&mut headers).is_err());
    }

    #[tokio::test]
    async fn length_and_deadline_preserved() {
        let body = timed_body(full("form"), 100, Instant::now() + Limits::duration(100));
        assert_eq!(body.size_hint().exact(), Some(4));
        assert_eq!(body.collect().await.unwrap().to_bytes(), "form");
        let body = timed_body(full("form"), 100, Instant::now());
        assert!(body.collect().await.is_err());
    }
}
