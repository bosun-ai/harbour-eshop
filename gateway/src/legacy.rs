//! HTTPS HTTP/1 adapter: no redirect following, decoding, pooling or retries.
use crate::{Body, Error, body, config::Config};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{
    HeaderMap, Request, Response,
    body::{Body as HttpBody, Frame, SizeHint},
    header,
};
use hyper_util::rt::TokioIo;
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    net::TcpStream,
    time::{Instant, Sleep, timeout},
};
use tokio_rustls::{TlsConnector, rustls::pki_types::ServerName};

/// Transport failure before response headers; stream failures close the response.
#[derive(Debug)]
pub enum Failure {
    Connection,
    Tls,
    Timeout,
    Protocol,
    TooLarge,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for Failure {}

/// Strip standard and Connection-nominated hop-by-hop fields.
pub fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(header::CONNECTION)
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
        "proxy-connection",
    ] {
        headers.remove(name);
    }
}

/// Deadline wrapper keeps size hints, allowing known-length bodies to stream.
pub struct TimedBody {
    inner: Body,
    idle: Duration,
    timer: Pin<Box<Sleep>>,
    end: Option<Instant>,
}
impl TimedBody {
    /// Limit inactivity and optionally total request-body duration.
    pub fn wrap(inner: Body, idle: Duration, total: Option<Duration>) -> Body {
        let end = total.map(|limit| Instant::now() + limit);
        let next = end.map_or(Instant::now() + idle, |end| end.min(Instant::now() + idle));
        Self {
            inner,
            idle,
            timer: Box::pin(tokio::time::sleep_until(next)),
            end,
        }
        .boxed_unsync()
    }
}
impl HttpBody for TimedBody {
    type Data = Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if self.inner.is_end_stream() {
            return Poll::Ready(None);
        }
        if self.timer.as_mut().poll(context).is_ready() {
            return Poll::Ready(Some(Err(Failure::Timeout.into())));
        }
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(Some(frame)) => {
                let next = self.end.map_or(Instant::now() + self.idle, |end| {
                    end.min(Instant::now() + self.idle)
                });
                self.timer.as_mut().reset(next);
                Poll::Ready(Some(frame))
            }
            other => other,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Stateless private legacy connection factory.
#[derive(Clone)]
pub struct Legacy {
    config: Config,
}
impl Legacy {
    /// Construct the adapter from already validated TLS and origin settings.
    pub fn new(config: Config) -> Self {
        Self { config }
    }
    /// Send exactly once. Unknown-length bodies are buffered up to 1 MiB.
    pub async fn send(&self, mut request: Request<Body>) -> Result<Response<Body>, Failure> {
        if request.uri().scheme().is_some() || !request.uri().path().starts_with('/') {
            return Err(Failure::Protocol);
        }
        let declared = request.headers().get(header::CONTENT_LENGTH).cloned();
        strip_hop_headers(request.headers_mut());
        // Never forward a client claim of proxy identity to future consumers.
        request.headers_mut().remove("forwarded");
        let forwarding: Vec<_> = request
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("x-forwarded-"))
            .cloned()
            .collect();
        for name in forwarding {
            request.headers_mut().remove(name);
        }
        if let Some(length) = declared {
            request.headers_mut().insert(header::CONTENT_LENGTH, length);
        }
        let (mut parts, incoming) = request.into_parts();
        parts.version = hyper::Version::HTTP_11;
        let incoming = TimedBody::wrap(
            incoming,
            self.config.body_idle,
            Some(self.config.request_body),
        );
        let outgoing = if parts.headers.contains_key(header::CONTENT_LENGTH) {
            incoming
        } else {
            let mut incoming = incoming;
            let mut bytes = Vec::new();
            while let Some(frame) = incoming.frame().await {
                let frame = frame.map_err(|_| Failure::Timeout)?;
                if let Ok(data) = frame.into_data() {
                    if bytes.len() + data.len() > 1024 * 1024 {
                        return Err(Failure::TooLarge);
                    }
                    bytes.extend_from_slice(&data);
                }
            }
            parts.headers.insert(
                header::CONTENT_LENGTH,
                bytes.len().to_string().parse().unwrap(),
            );
            body(bytes)
        };
        let host = self
            .config
            .upstream
            .host()
            .unwrap()
            .trim_matches(['[', ']']);
        let port = self.config.upstream.port_u16().unwrap_or(443);
        let stream = timeout(self.config.connect, TcpStream::connect((host, port)))
            .await
            .map_err(|_| Failure::Timeout)?
            .map_err(|_| Failure::Connection)?;
        let name = ServerName::try_from(host.to_owned()).map_err(|_| Failure::Tls)?;
        let stream = timeout(
            self.config.connect,
            TlsConnector::from(self.config.client_tls.clone()).connect(name, stream),
        )
        .await
        .map_err(|_| Failure::Timeout)?
        .map_err(|_| Failure::Tls)?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| Failure::Protocol)?;
        // A connection is dedicated to this request; dropping its body closes it.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut response = timeout(
            self.config.upstream_response,
            sender.send_request(Request::from_parts(parts, outgoing)),
        )
        .await
        .map_err(|_| Failure::Timeout)?
        .map_err(|_| Failure::Protocol)?;
        strip_hop_headers(response.headers_mut());
        Ok(response.map(|incoming| {
            TimedBody::wrap(
                incoming
                    .map_err(|error| -> Error { error.into() })
                    .boxed_unsync(),
                self.config.body_idle,
                None,
            )
        }))
    }
    /// Strict TLS readiness with a total deadline and bounded expected body.
    pub fn ready(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            timeout(self.config.connect, async {
                let request = Request::builder()
                    .uri("/hello")
                    .header(
                        header::HOST,
                        self.config.upstream.authority().unwrap().as_str(),
                    )
                    .body(body(""))
                    .unwrap();
                let Ok(response) = self.send(request).await else {
                    return false;
                };
                if response.status() != 200 {
                    return false;
                }
                let mut incoming = response.into_body();
                let mut bytes = Vec::new();
                while let Some(frame) = incoming.frame().await {
                    let Ok(frame) = frame else {
                        return false;
                    };
                    if let Ok(data) = frame.into_data() {
                        if bytes.len() + data.len() > 6 {
                            return false;
                        }
                        bytes.extend_from_slice(&data);
                    }
                }
                bytes == b"Hello!"
            })
            .await
            .unwrap_or(false)
        })
    }
}
