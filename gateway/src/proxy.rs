use std::{convert::Infallible, future::Future, net::IpAddr, time::Duration};

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{
    body::Incoming,
    header::{CONNECTION, HOST},
    Request, Response, StatusCode, Uri,
};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use pin_project_lite::pin_project;
use tokio::time::{self, Sleep};
use url::Url;
use uuid::Uuid;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type GatewayBody = BoxBody<Bytes, BoxError>;
type HttpsClient = Client<HttpsConnector<HttpConnector>, Incoming>;

#[derive(Clone)]
pub struct LegacyUpstream {
    client: HttpsClient,
    origin: Url,
    request_timeout: Duration,
    response_idle_timeout: Duration,
}

impl LegacyUpstream {
    pub fn new(
        origin: Url,
        tls: rustls::ClientConfig,
        connect_timeout: Duration,
        request_timeout: Duration,
        response_idle_timeout: Duration,
    ) -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        connector.set_connect_timeout(Some(connect_timeout));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(connector);
        Self {
            client: Client::builder(TokioExecutor::new()).build(https),
            origin,
            request_timeout,
            response_idle_timeout,
        }
    }

    pub async fn forward(
        &self,
        mut request: Request<Incoming>,
        client_ip: IpAddr,
    ) -> Response<GatewayBody> {
        let uri = match upstream_uri(&self.origin, request.uri()) {
            Ok(uri) => uri,
            Err(()) => return error_response(StatusCode::BAD_REQUEST, "invalid request target"),
        };
        let request_id = Uuid::new_v4().to_string();
        remove_hop_by_hop(request.headers_mut());
        request.headers_mut().remove(HOST);
        request.headers_mut().remove("x-forwarded-for");
        request.headers_mut().remove("x-forwarded-proto");
        request.headers_mut().insert(
            "x-request-id",
            request_id.parse().expect("UUID is a valid header value"),
        );
        request.headers_mut().insert(
            "x-forwarded-for",
            client_ip
                .to_string()
                .parse()
                .expect("IP address is a valid header value"),
        );
        request.headers_mut().insert(
            "x-forwarded-proto",
            "https".parse().expect("constant is a valid header value"),
        );
        *request.uri_mut() = uri;

        let response = match time::timeout(self.request_timeout, self.client.request(request)).await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(request_id, error = %error, "legacy upstream request failed");
                return error_response(StatusCode::BAD_GATEWAY, "legacy upstream unavailable");
            }
            Err(_) => {
                tracing::warn!(request_id, "legacy upstream request timed out");
                return error_response(StatusCode::GATEWAY_TIMEOUT, "legacy upstream timed out");
            }
        };
        let (parts, body) = response.into_parts();
        let mut response = Response::from_parts(
            parts,
            IdleTimeoutBody::new(body, self.response_idle_timeout).boxed(),
        );
        remove_hop_by_hop(response.headers_mut());
        response
    }
}

fn upstream_uri(origin: &Url, incoming: &Uri) -> Result<Uri, ()> {
    let authority = origin.authority();
    let path = incoming
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/");
    format!("https://{authority}{path}").parse().map_err(|_| ())
}

fn remove_hop_by_hop(headers: &mut hyper::HeaderMap) {
    let nominated = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse::<hyper::header::HeaderName>().ok())
        .collect::<Vec<_>>();
    for name in nominated {
        headers.remove(name);
    }
    headers.remove(CONNECTION);
    for name in [
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

fn error_response(status: StatusCode, message: &'static str) -> Response<GatewayBody> {
    Response::builder()
        .status(status)
        .body(
            Full::new(Bytes::from(message))
                .map_err(|never: Infallible| match never {})
                .boxed(),
        )
        .expect("fixed error response")
}

pin_project! {
    struct IdleTimeoutBody { #[pin] inner: Incoming, #[pin] deadline: Sleep, timeout: Duration }
}
impl IdleTimeoutBody {
    fn new(inner: Incoming, timeout: Duration) -> Self {
        Self {
            inner,
            deadline: time::sleep(timeout),
            timeout,
        }
    }
}
impl Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(context) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                this.deadline.reset(time::Instant::now() + *this.timeout);
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                std::task::Poll::Ready(Some(Err(Box::new(error))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending if this.deadline.poll(context).is_ready() => {
                std::task::Poll::Ready(Some(Err("legacy upstream response became idle".into())))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
