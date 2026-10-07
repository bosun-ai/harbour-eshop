use crate::config::{Config, ConfigError};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    HeaderMap, Request, Response, StatusCode,
    body::{Body as HttpBody, Frame, Incoming},
    header::{CONNECTION, CONTENT_LENGTH, TRANSFER_ENCODING},
};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    net::TcpStream,
    time::{Instant, Sleep, timeout},
};
use tokio_rustls::TlsConnector;

pub type Body = UnsyncBoxBody<Bytes, ConfigError>;

#[derive(Debug)]
pub enum ProxyError {
    Protocol,
    Upstream,
    Timeout,
}

impl ProxyError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Upstream => "upstream",
            Self::Timeout => "timeout",
        }
    }
}

pub fn text(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from_static(message.as_bytes()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
}

/// Remove connection-scoped fields without collapsing repeated end-to-end fields.
fn strip_hop(headers: &mut HeaderMap) {
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

/// Bound each streaming body by both idle time and the whole exchange deadline.
struct TimedBody {
    inner: Incoming,
    timer: Pin<Box<Sleep>>,
    deadline: Instant,
    idle: Duration,
}

impl TimedBody {
    fn new(inner: Incoming, idle: Duration, deadline: Instant) -> Self {
        Self {
            inner,
            timer: Box::pin(tokio::time::sleep_until(
                (Instant::now() + idle).min(deadline),
            )),
            deadline,
            idle,
        }
    }
}

impl HttpBody for TimedBody {
    type Data = Bytes;
    type Error = ConfigError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ConfigError>>> {
        if self.timer.as_mut().poll(context).is_ready() {
            return Poll::Ready(Some(Err(
                "body timeout; delivery outcome may be unknown".into()
            )));
        }
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                let next = (Instant::now() + self.idle).min(self.deadline);
                self.timer.as_mut().reset(next);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(_))) => Poll::Ready(Some(Err("body transport error".into()))),
            other => other.map(|frame| {
                frame.map(|result| result.map_err(|error| Box::new(error) as ConfigError))
            }),
        }
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Clone)]
pub struct LegacyUpstream {
    pub config: Arc<Config>,
}

impl LegacyUpstream {
    /// Open one verified TLS connection and send exactly one request. Never retry.
    async fn exchange(
        &self,
        request: Request<Body>,
        deadline: Instant,
    ) -> Result<Response<Body>, ProxyError> {
        let config = &self.config;
        let connect = async {
            let tcp = TcpStream::connect((
                config.upstream.host().unwrap(),
                config.upstream.port_u16().unwrap_or(443),
            ))
            .await
            .map_err(|_| ProxyError::Upstream)?;
            let name = ServerName::try_from(config.upstream.host().unwrap().to_owned())
                .map_err(|_| ProxyError::Upstream)?;
            TlsConnector::from(config.client_tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|_| ProxyError::Upstream)
        };
        let tls = timeout(config.connect, connect)
            .await
            .map_err(|_| ProxyError::Timeout)??;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|_| ProxyError::Upstream)?;
        let idle = config.idle;
        tokio::spawn(async move {
            let _ = tokio::time::timeout_at(deadline, connection).await;
        });
        let mut response = tokio::time::timeout_at(
            (Instant::now() + idle).min(deadline),
            sender.send_request(request),
        )
        .await
        .map_err(|_| ProxyError::Timeout)?
        .map_err(|_| ProxyError::Upstream)?;
        strip_hop(response.headers_mut());
        Ok(response.map(|body| TimedBody::new(body, idle, deadline).boxed_unsync()))
    }

    pub async fn forward(
        &self,
        mut request: Request<Incoming>,
    ) -> Result<Response<Body>, ProxyError> {
        if request.method() == hyper::Method::CONNECT
            || request.uri().scheme().is_some()
            || request.headers().contains_key("upgrade")
        {
            return Err(ProxyError::Protocol);
        }
        if request.headers().contains_key(TRANSFER_ENCODING)
            && request.headers().contains_key(CONTENT_LENGTH)
        {
            return Err(ProxyError::Protocol);
        }
        if request
            .headers()
            .get_all(TRANSFER_ENCODING)
            .iter()
            .any(|value| value.as_bytes() != b"chunked")
            || request.headers().get_all(CONNECTION).iter().any(|value| {
                value.to_str().map_or(true, |value| {
                    value.split(',').any(|name| {
                        ["host", "content-length", "transfer-encoding"]
                            .iter()
                            .any(|protected| name.trim().eq_ignore_ascii_case(protected))
                    })
                })
            })
        {
            return Err(ProxyError::Protocol);
        }
        let chunked = request.headers().contains_key(TRANSFER_ENCODING);
        strip_hop(request.headers_mut());
        // Retain chunked framing: Harbour deliberately does not parse this as a form.
        if chunked {
            request
                .headers_mut()
                .insert(TRANSFER_ENCODING, "chunked".parse().unwrap());
        }
        let deadline = Instant::now() + self.config.total;
        let idle = self.config.idle;
        self.exchange(
            request.map(|body| TimedBody::new(body, idle, deadline).boxed_unsync()),
            deadline,
        )
        .await
    }

    pub async fn check_legacy(&self) -> bool {
        let request = Request::builder()
            .uri("/hello")
            .header("host", self.config.upstream.authority().unwrap().as_str())
            .body(
                Full::new(Bytes::new())
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            )
            .unwrap();
        let probe = async {
            let response = self
                .exchange(request, Instant::now() + self.config.connect)
                .await
                .ok()?;
            if response.status() != StatusCode::OK {
                return None;
            }
            // A fixed probe body, not an unbounded application-response collector.
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.ok()?;
                if let Ok(data) = frame.into_data() {
                    if bytes.len() + data.len() > 6 {
                        return None;
                    }
                    bytes.extend_from_slice(&data);
                }
            }
            (bytes == b"Hello!").then_some(())
        };
        matches!(timeout(self.config.connect, probe).await, Ok(Some(())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_headers_survive_hop_removal() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, "x-private, keep-alive".parse().unwrap());
        headers.insert("x-private", "remove".parse().unwrap());
        headers.append("set-cookie", "a=1".parse().unwrap());
        headers.append("set-cookie", "b=2".parse().unwrap());
        strip_hop(&mut headers);
        assert!(!headers.contains_key("x-private"));
        assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
    }
}
