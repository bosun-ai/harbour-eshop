//! Opaque, no-retry HTTP/1.1 HTTPS adapter to the fixed Harbour destination.
use crate::{
    Body,
    config::{Config, duration},
    dispatch::{HttpHandler, RequestContext, TransportFailure},
    server::GuardedBody,
};
use http_body_util::BodyExt;
use hyper::{
    Request, Response, Uri,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use hyper_util::rt::TokioIo;
use rustls::{ClientConfig, pki_types::ServerName};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::{net::TcpStream, time::timeout};
use tokio_rustls::TlsConnector;

/// Independent TLS client with no redirects, retries, pooling or decompression.
pub struct LegacyUpstream {
    host: String,
    tls: Arc<ClientConfig>,
    connect_ms: u64,
    idle_ms: u64,
    header_ms: u64,
}
impl LegacyUpstream {
    /// Build verified upstream TLS once, before either listener is opened.
    pub fn new(config: &Config) -> Result<Self, &'static str> {
        let uri: Uri = config.legacy_upstream.parse().map_err(|_| "upstream_uri")?;
        let host = uri
            .host()
            .ok_or("upstream_uri")?
            .trim_matches(['[', ']'])
            .to_owned();
        ServerName::try_from(host.clone()).map_err(|_| "upstream_name")?;
        Ok(Self {
            host,
            tls: config.client_tls()?,
            connect_ms: config.connect_ms,
            idle_ms: config.body_idle_ms,
            header_ms: config.header_ms,
        })
    }
}
impl HttpHandler for LegacyUpstream {
    fn handle(
        &self,
        mut request: Request<Body>,
        context: RequestContext,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, TransportFailure>> + Send + '_>> {
        Box::pin(async move {
            strip_hop_headers(request.headers_mut());
            *request.version_mut() = hyper::Version::HTTP_11;
            // Origin form preserves raw encoding; client authority never selects a destination.
            *request.uri_mut() = request
                .uri()
                .path_and_query()
                .ok_or(TransportFailure::Framing)?
                .as_str()
                .parse()
                .map_err(|_| TransportFailure::Framing)?;
            request
                .headers_mut()
                .insert("connection", HeaderValue::from_static("close"));
            let connect = async {
                let tcp = TcpStream::connect((self.host.as_str(), 8002))
                    .await
                    .map_err(|_| TransportFailure::Upstream)?;
                let name = ServerName::try_from(self.host.clone())
                    .map_err(|_| TransportFailure::Upstream)?;
                TlsConnector::from(self.tls.clone())
                    .connect(name, tcp)
                    .await
                    .map_err(|_| TransportFailure::Upstream)
            };
            let tls = timeout(duration(self.connect_ms), connect)
                .await
                .map_err(|_| TransportFailure::Deadline)??;
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
                .await
                .map_err(|_| TransportFailure::Upstream)?;
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let guard = DriverGuard(driver.abort_handle());
            let mut response = timeout(duration(self.header_ms), sender.send_request(request))
                .await
                .map_err(|_| TransportFailure::Deadline)?
                .map_err(|_| TransportFailure::Upstream)?;
            strip_hop_headers(response.headers_mut());
            let (parts, body) = response.into_parts();
            let body = GuardedBody::new(
                body.map_err(|_| TransportFailure::Upstream).boxed_unsync(),
                context,
                self.idle_ms,
                None,
                Some(guard),
            );
            Ok(Response::from_parts(parts, body.boxed_unsync()))
        })
    }
}

/// Dropping a request/response cancels its private upstream connection driver.
pub struct DriverGuard(pub tokio::task::AbortHandle);
impl Drop for DriverGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Remove standard hop fields and every field nominated by Connection.
pub fn strip_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<_> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
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
