use std::{
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, Uri,
    body::{Body as HttpBody, Frame, SizeHint},
    header::{CONNECTION, HeaderMap, HeaderName, HeaderValue},
};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::{
    net::TcpStream,
    time::{Sleep, sleep, timeout},
};
use tokio_rustls::TlsConnector;

use crate::{
    config::Config,
    dispatch::{Context, Handler, HandlerResult},
};

pub type Body = UnsyncBoxBody<Bytes, TransportError>;

#[derive(Debug, Clone, Copy)]
pub enum TransportError {
    Timeout,
    Upstream,
    Client,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for TransportError {}

pub fn full(text: impl Into<Bytes>) -> Body {
    Full::new(text.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Absolute body deadline, retaining the original size hint for Content-Length framing.
struct TimedBody<B> {
    inner: Pin<Box<B>>,
    deadline: Pin<Box<Sleep>>,
}

impl<B: HttpBody<Data = Bytes, Error = TransportError>> HttpBody for TimedBody<B> {
    type Data = Bytes;
    type Error = TransportError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.deadline.as_mut().poll(context).is_ready() {
            return Poll::Ready(Some(Err(TransportError::Timeout)));
        }
        self.inner.as_mut().poll_frame(context)
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

pub fn timed(body: Body, limit: Duration) -> Body {
    TimedBody {
        inner: Box::pin(body),
        deadline: Box::pin(sleep(limit)),
    }
    .boxed_unsync()
}

pub fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
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
    config: Arc<Config>,
}

impl LegacyUpstream {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    async fn forward(
        config: Arc<Config>,
        mut request: Request<Body>,
        context: Context,
    ) -> Result<Response<Body>, TransportError> {
        let host = config
            .upstream
            .host()
            .ok_or(TransportError::Upstream)?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = config.upstream.port_u16().unwrap_or(443);
        let name = ServerName::try_from(host.to_owned()).map_err(|_| TransportError::Upstream)?;
        let stream = timeout(config.connect, async {
            let tcp = TcpStream::connect((host, port))
                .await
                .map_err(|_| TransportError::Upstream)?;
            TlsConnector::from(config.upstream_tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|_| TransportError::Upstream)
        })
        .await
        .map_err(|_| TransportError::Timeout)??;
        // A fresh connection and one send_request: no redirect, pooling, or retry machinery.
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| TransportError::Upstream)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str());
        *request.uri_mut() = path.parse().map_err(|_| TransportError::Client)?;
        strip_hop_headers(request.headers_mut());
        let spoofed: Vec<_> = request
            .headers()
            .keys()
            .filter(|name| {
                name.as_str() == "forwarded"
                    || name.as_str().starts_with("x-forwarded-")
                    || name.as_str() == "x-request-id"
            })
            .cloned()
            .collect();
        for name in spoofed {
            request.headers_mut().remove(name);
        }
        request.headers_mut().insert(
            "x-forwarded-for",
            HeaderValue::from_str(&context.peer.ip().to_string()).unwrap(),
        );
        request
            .headers_mut()
            .insert("x-forwarded-proto", HeaderValue::from_static("https"));
        request.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&context.correlation_id.to_string()).unwrap(),
        );
        let response = timeout(config.upstream_timeout, sender.send_request(request))
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|_| TransportError::Upstream)?;
        let (mut parts, body) = response.into_parts();
        strip_hop_headers(&mut parts.headers);
        Ok(Response::from_parts(
            parts,
            timed(
                body.map_err(|_| TransportError::Upstream).boxed_unsync(),
                config.upstream_timeout,
            ),
        ))
    }

    pub async fn ready(&self) -> bool {
        let request = Request::builder()
            .uri(Uri::from_static("/hello"))
            .header("host", self.config.upstream.authority().unwrap().as_str())
            .body(full(""))
            .unwrap();
        let probe = async {
            let response = Self::forward(
                self.config.clone(),
                request,
                Context {
                    peer: "127.0.0.1:0".parse().unwrap(),
                    correlation_id: 0,
                },
            )
            .await
            .ok()?;
            if response.status() != 200 {
                return None;
            }
            let body = response.into_body().collect().await.ok()?.to_bytes();
            (body == "Hello!").then_some(())
        };
        matches!(timeout(Duration::from_secs(3), probe).await, Ok(Some(())))
    }
}

impl Handler for LegacyUpstream {
    fn handle(&self, request: Request<Body>, context: Context) -> HandlerResult {
        Box::pin(Self::forward(self.config.clone(), request, context))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remove_nominated_headers_preserve_cookie_fields() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "connection",
            HeaderValue::from_static("x-private, keep-alive"),
        );
        headers.insert("x-private", HeaderValue::from_static("secret"));
        headers.append("set-cookie", HeaderValue::from_static("one=1"));
        headers.append("set-cookie", HeaderValue::from_static("two=2"));
        strip_hop_headers(&mut headers);
        assert!(!headers.contains_key("x-private"));
        assert!(!headers.contains_key("connection"));
        assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
    }
    #[tokio::test]
    async fn body_wrapper_keeps_fixed_length() {
        let body = timed(full("user=alice"), Duration::from_secs(1));
        assert_eq!(body.size_hint().exact(), Some(10));
        assert_eq!(body.collect().await.unwrap().to_bytes(), "user=alice");
    }
}
