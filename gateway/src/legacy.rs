//! Request-scoped, verified HTTP/1 forwarding with no retries or redirects.
use crate::{
    Body, Error,
    config::Config,
    dispatch::{Context, Handler},
};
use http_body_util::BodyExt;
use hyper::{HeaderMap, Request, Response, header};
use hyper_util::rt::TokioIo;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, rustls::pki_types::ServerName};

/// Strip connection-local fields, including arbitrary Connection nominations.
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
    ] {
        headers.remove(name);
    }
}

/// The only application owner compiled into the bootstrap.
pub struct LegacyUpstream(pub Arc<Config>);

struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct UpstreamBody {
    incoming: hyper::body::Incoming,
    _driver: Driver,
}
impl hyper::body::Body for UpstreamBody {
    type Data = bytes::Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        task: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Error>>> {
        Pin::new(&mut self.incoming)
            .poll_frame(task)
            .map(|frame| frame.map(|result| result.map_err(|error| -> Error { Box::new(error) })))
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.incoming.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.incoming.is_end_stream()
    }
}

impl Handler for LegacyUpstream {
    fn handle(
        &self,
        mut request: Request<Body>,
        _: Context,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send + '_>> {
        Box::pin(async move {
            let config = &self.0;
            let host = config.upstream.host().ok_or("missing upstream host")?;
            let port = config.upstream.port_u16().unwrap_or(443);
            let connect = async {
                let tcp = TcpStream::connect((host.trim_matches(['[', ']']), port)).await?;
                let name = ServerName::try_from(host.trim_matches(['[', ']']).to_owned())?;
                let tls = TlsConnector::from(config.private_tls.clone())
                    .connect(name, tcp)
                    .await?;
                Ok::<_, Error>(tls)
            };
            let tls = tokio::time::timeout(config.connect, connect)
                .await
                .map_err(|_| crate::server::Deadline)??;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
            // The deadline bounds the driver too, including unread request bytes.
            let exchange = config.exchange + config.connect;
            let driver = Driver(tokio::spawn(async move {
                let _ = tokio::time::timeout(exchange, connection).await;
            }));
            *request.uri_mut() = request
                .uri()
                .path_and_query()
                .ok_or("missing path")?
                .as_str()
                .parse()?;
            strip_hop_headers(request.headers_mut());
            request.headers_mut().insert(
                header::CONNECTION,
                hyper::header::HeaderValue::from_static("close"),
            );
            let response = sender.send_request(request).await?;
            let (mut parts, incoming) = response.into_parts();
            strip_hop_headers(&mut parts.headers);
            Ok(Response::from_parts(
                parts,
                UpstreamBody {
                    incoming,
                    _driver: driver,
                }
                .boxed_unsync(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_only_hop_headers_and_preserves_multiplicity() {
        let mut headers = HeaderMap::new();
        headers.append("connection", "x-private, Keep-Alive".parse().unwrap());
        headers.append("connection", "upgrade".parse().unwrap());
        headers.insert("x-private", "secret".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("host", "browser.example".parse().unwrap());
        headers.append("set-cookie", "a=1".parse().unwrap());
        headers.append("set-cookie", "b=2".parse().unwrap());
        strip_hop_headers(&mut headers);
        assert!(!headers.contains_key("x-private"));
        assert!(!headers.contains_key("connection"));
        assert_eq!(headers["host"], "browser.example");
        assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
    }
}
