//! One verified TLS connection per request: no pool, redirect, decompression or retry.
use crate::{
    Body, Error,
    config::{Config, upstream_port},
    response,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{
    HeaderMap, Request, Response, StatusCode,
    body::{Body as HttpBody, Frame, SizeHint},
    header,
};
use hyper_util::rt::TokioIo;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    net::TcpStream,
    task::AbortHandle,
    time::{Sleep, timeout},
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

#[derive(Clone)]
pub struct LegacyUpstream {
    host: String,
    port: u16,
    authority: String,
    tls: TlsConnector,
    connect: Duration,
    headers: Duration,
    idle: Duration,
}

pub fn strip_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(|name| name.trim().to_owned()))
        .collect();
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
}

// The timer is reset only by a body frame, not by repeated polls. Dropping a
// response also drops its single-use upstream connection driver.
#[derive(Debug)]
struct StreamIdleTimeout;
impl std::fmt::Display for StreamIdleTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stream idle timeout")
    }
}
impl std::error::Error for StreamIdleTimeout {}

struct IdleBody<B> {
    inner: B,
    timer: Pin<Box<Sleep>>,
    idle: Duration,
    driver: Option<AbortHandle>,
}
struct DriverGuard(Option<AbortHandle>);
impl Drop for DriverGuard {
    fn drop(&mut self) {
        if let Some(driver) = &self.0 {
            driver.abort();
        }
    }
}
impl<B> Drop for IdleBody<B> {
    fn drop(&mut self) {
        if let Some(driver) = &self.driver {
            driver.abort();
        }
    }
}
impl<B: HttpBody<Data = Bytes> + Unpin> HttpBody for IdleBody<B>
where
    B::Error: Into<Error>,
{
    type Data = Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(frame) => {
                let deadline = tokio::time::Instant::now() + self.idle;
                self.timer.as_mut().reset(deadline);
                Poll::Ready(frame.map(|result| result.map_err(Into::into)))
            }
            Poll::Pending => {
                if self.timer.as_mut().poll(context).is_ready() {
                    tracing::warn!(failure = "stream_idle", "body terminated");
                    Poll::Ready(Some(Err(StreamIdleTimeout.into())))
                } else {
                    Poll::Pending
                }
            }
        }
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

pub fn bounded_body<B>(body: B, idle: Duration, driver: Option<AbortHandle>) -> Body
where
    B: HttpBody<Data = Bytes> + Unpin + Send + 'static,
    B::Error: Into<Error>,
{
    IdleBody {
        inner: body,
        timer: Box::pin(tokio::time::sleep(idle)),
        idle,
        driver,
    }
    .boxed_unsync()
}

impl LegacyUpstream {
    pub fn new(config: &Config) -> Result<Self, Error> {
        let mut roots = RootCertStore::empty();
        for cert in &config.ca {
            roots
                .add(cert.clone())
                .map_err(|_| "invalid upstream trust certificate")?;
        }
        let tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            host: config
                .origin
                .host()
                .unwrap()
                .trim_matches(['[', ']'])
                .into(),
            port: upstream_port(&config.origin)?,
            authority: config.origin.authority().unwrap().to_string(),
            tls: TlsConnector::from(Arc::new(tls)),
            connect: config.connect,
            headers: config.headers,
            idle: config.idle,
        })
    }

    pub async fn forward(&self, mut request: Request<Body>) -> Response<Body> {
        // Harbour does not decode chunked bodies. Reject rather than dechunk and
        // accidentally activate previously unsupported writes.
        if request.headers().contains_key(header::TRANSFER_ENCODING) {
            return response(
                StatusCode::NOT_IMPLEMENTED,
                "Chunked requests are not supported\n",
            );
        }
        strip_hop_headers(request.headers_mut());
        request.headers_mut().remove(header::EXPECT);
        *request.version_mut() = hyper::Version::HTTP_11;
        let path = request
            .uri()
            .path_and_query()
            .map(|path| path.as_str())
            .unwrap_or("/");
        let Ok(uri) = path.parse() else {
            return response(StatusCode::BAD_REQUEST, "Invalid request target\n");
        };
        *request.uri_mut() = uri;
        let connected = timeout(self.connect, async {
            let tcp = TcpStream::connect((self.host.as_str(), self.port)).await?;
            let name = ServerName::try_from(self.host.clone())?;
            let tls = self.tls.connect(name, tcp).await?;
            let pair = hyper::client::conn::http1::Builder::new()
                .title_case_headers(true)
                .handshake(TokioIo::new(tls))
                .await?;
            Ok::<_, Error>(pair)
        })
        .await;
        let (mut sender, connection) = match connected {
            Ok(Ok(pair)) => pair,
            Ok(Err(_)) => return failure(false, "upstream_connect"),
            Err(_) => return failure(true, "connect_timeout"),
        };
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        // Cancellation while waiting for headers must not detach the upstream.
        let mut guard = DriverGuard(Some(driver.abort_handle()));
        let result = timeout(self.headers, sender.send_request(request)).await;
        match result {
            Ok(Ok(mut upstream)) => {
                strip_hop_headers(upstream.headers_mut());
                upstream.map(|body| bounded_body(body, self.idle, guard.0.take()))
            }
            Ok(Err(error)) => {
                driver.abort();
                protocol_failure(&error)
            }
            Err(_) => {
                driver.abort();
                failure(true, "header_timeout")
            }
        }
    }

    pub async fn ready(&self) -> bool {
        let request = Request::builder()
            .uri("/hello")
            .header(header::HOST, &self.authority)
            .body(response(StatusCode::OK, "").into_body())
            .unwrap();
        let check = async {
            let response = self.forward(request).await;
            if response.status() != StatusCode::OK {
                return false;
            }
            // Readiness is bounded in bytes as well as time.
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(frame) = body.frame().await {
                let Ok(frame) = frame else {
                    return false;
                };
                if let Ok(data) = frame.into_data() {
                    bytes.extend_from_slice(&data);
                }
                if bytes.len() > 6 {
                    return false;
                }
            }
            bytes == b"Hello!"
        };
        timeout(self.connect + self.headers + self.idle, check)
            .await
            .unwrap_or(false)
    }
}

fn protocol_failure(error: &(dyn std::error::Error + 'static)) -> Response<Body> {
    // Hyper wraps upload-body failures; preserve the cause before headers.
    let mut cause = Some(error);
    while let Some(error) = cause {
        if error.is::<StreamIdleTimeout>() {
            return failure(true, "stream_idle");
        }
        cause = error.source();
    }
    failure(false, "upstream_protocol")
}

fn failure(timed_out: bool, category: &'static str) -> Response<Body> {
    tracing::warn!(failure = category, "legacy request failed");
    response(
        if timed_out {
            StatusCode::GATEWAY_TIMEOUT
        } else {
            StatusCode::BAD_GATEWAY
        },
        "Legacy upstream unavailable\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_connection_tokens_preserves_repeated_cookies() {
        let mut headers = HeaderMap::new();
        headers.append("connection", "x-private, Keep-Alive".parse().unwrap());
        headers.append("connection", "x-another".parse().unwrap());
        for name in ["x-private", "x-another", "keep-alive", "transfer-encoding"] {
            headers.insert(name, "test".parse().unwrap());
        }
        headers.append("set-cookie", "a=1; path=/".parse().unwrap());
        headers.append("set-cookie", "b=2; Max-Age=0".parse().unwrap());
        headers.insert("host", "shop.example".parse().unwrap());
        strip_hop_headers(&mut headers);
        assert_eq!(headers.len(), 3);
        assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
        assert_eq!(headers["host"], "shop.example");
    }

    #[tokio::test]
    async fn idle_stream_times_out_and_drop_aborts_driver() {
        struct Pending;
        impl HttpBody for Pending {
            type Data = Bytes;
            type Error = Error;
            fn poll_frame(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
                Poll::Pending
            }
        }
        let driver = tokio::spawn(std::future::pending::<()>());
        let mut body = bounded_body(
            Pending,
            Duration::from_millis(5),
            Some(driver.abort_handle()),
        );
        let error = body.frame().await.unwrap().unwrap_err();
        assert!(error.is::<StreamIdleTimeout>());
        assert_eq!(
            protocol_failure(error.as_ref()).status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            protocol_failure(&std::io::Error::other("protocol failure")).status(),
            StatusCode::BAD_GATEWAY
        );
        drop(body);
        assert!(driver.await.unwrap_err().is_cancelled());
        let mut body = bounded_body(
            response(StatusCode::OK, "bytes").into_body(),
            Duration::from_secs(1),
            None,
        );
        assert_eq!(body.size_hint().exact(), Some(5));
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "bytes"
        );
        assert!(body.frame().await.is_none());
    }
}
