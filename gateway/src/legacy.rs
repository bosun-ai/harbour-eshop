//! One verified TLS connection per request: no pooling retries or application state.
use crate::{Body, Error, config::Config};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{HeaderMap, HeaderName},
};
use hyper_util::rt::TokioIo;
use std::{fs::File, io::BufReader, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout},
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

#[derive(Clone)]
pub(crate) struct Legacy {
    host: String,
    port: u16,
    tls: TlsConnector,
    connect: Duration,
    response: Duration,
}

pub(crate) fn certificates(
    path: &str,
) -> Result<Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>, Error> {
    let certificates = rustls_pemfile::certs(&mut BufReader::new(File::open(path)?))
        .collect::<Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        return Err("empty certificate file".into());
    }
    Ok(certificates)
}

pub(crate) fn text(status: StatusCode, value: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(
            Full::new(Bytes::from_static(value.as_bytes()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}

pub(crate) fn strip_hop(headers: &mut HeaderMap) {
    let nominated: Vec<HeaderName> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| value.trim().parse().ok())
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

pub(crate) fn strip_untrusted(headers: &mut HeaderMap) {
    let names: Vec<_> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            name == "forwarded"
                || name.starts_with("x-forwarded-")
                || matches!(
                    name,
                    "x-real-ip" | "x-request-id" | "true-client-ip" | "cf-connecting-ip"
                )
        })
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
}

pub(crate) fn incoming(body: Incoming, deadline: Instant) -> Body {
    let stream = http_body_util::BodyStream::new(body);
    // A body deadline bounds uploads and slow/truncated upstream responses without buffering.
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    struct Timed {
        stream: http_body_util::BodyStream<Incoming>,
        timer: Pin<Box<tokio::time::Sleep>>,
    }
    impl hyper::body::Body for Timed {
        type Data = Bytes;
        type Error = Error;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Error>>> {
            use std::future::Future;
            if self.timer.as_mut().poll(context).is_ready() {
                return Poll::Ready(Some(Err("body deadline exceeded".into())));
            }
            hyper::body::Body::poll_frame(Pin::new(&mut self.stream), context)
                .map(|frame| frame.map(|value| value.map_err(Into::into)))
        }
        fn size_hint(&self) -> hyper::body::SizeHint {
            hyper::body::Body::size_hint(&self.stream)
        }
        fn is_end_stream(&self) -> bool {
            hyper::body::Body::is_end_stream(&self.stream)
        }
    }
    Timed {
        stream,
        timer: Box::pin(tokio::time::sleep_until(deadline)),
    }
    .boxed_unsync()
}

impl Legacy {
    pub(crate) fn new(config: &Config) -> Result<Self, Error> {
        let mut roots = RootCertStore::empty();
        for certificate in certificates(&config.ca)? {
            roots.add(certificate)?;
        }
        let tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            host: config.origin.host().unwrap().to_owned(),
            port: config.origin.port_u16().unwrap_or(443),
            tls: TlsConnector::from(Arc::new(tls)),
            connect: config.connect,
            response: config.response,
        })
    }

    pub(crate) async fn forward(
        &self,
        mut request: Request<Body>,
    ) -> Result<Response<Body>, StatusCode> {
        strip_hop(request.headers_mut());
        let path = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
            .parse()
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        *request.uri_mut() = path;
        *request.version_mut() = hyper::Version::HTTP_11;
        request
            .headers_mut()
            .insert("connection", "close".parse().unwrap());
        let connection = timeout(self.connect, async {
            let tcp = TcpStream::connect((self.host.as_str(), self.port)).await?;
            let tls = self
                .tls
                .connect(ServerName::try_from(self.host.clone())?, tcp)
                .await?;
            let (sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
            Ok::<_, Error>((sender, connection))
        })
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let (mut sender, connection) = connection;
        let deadline = Instant::now() + self.response;
        let response_timeout = self.response;
        let task = tokio::spawn(async move {
            let _ = timeout(response_timeout, connection).await;
        });
        // Dropping a cancelled request must also stop its private upstream connection.
        struct Abort(tokio::task::JoinHandle<()>);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let guard = Abort(task);
        let response = timeout(self.response, sender.send_request(request))
            .await
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let (mut parts, body) = response.into_parts();
        strip_hop(&mut parts.headers);
        // Keep the connection driver alive until the downstream body completes or is dropped.
        let body = incoming(body, deadline);
        struct Owned {
            body: Body,
            _guard: Abort,
        }
        impl hyper::body::Body for Owned {
            type Data = Bytes;
            type Error = Error;
            fn poll_frame(
                mut self: std::pin::Pin<&mut Self>,
                context: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Error>>> {
                std::pin::Pin::new(&mut self.body).poll_frame(context)
            }
            fn size_hint(&self) -> hyper::body::SizeHint {
                self.body.size_hint()
            }
            fn is_end_stream(&self) -> bool {
                self.body.is_end_stream()
            }
        }
        Ok(Response::from_parts(
            parts,
            Owned {
                body,
                _guard: guard,
            }
            .boxed_unsync(),
        ))
    }

    pub(crate) async fn ready(&self) -> bool {
        let request = Request::builder()
            .uri("/hello")
            .header("host", &self.host)
            .body(
                Full::new(Bytes::new())
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            )
            .unwrap();
        match self.forward(request).await {
            Ok(response) if response.status() == StatusCode::OK => response
                .into_body()
                .collect()
                .await
                .is_ok_and(|body| body.to_bytes() == "Hello!"),
            _ => false,
        }
    }
}
