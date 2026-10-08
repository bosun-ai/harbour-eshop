use std::{
    collections::HashSet,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use http_body_util::BodyExt;
use hyper::{Method, Request, Response, StatusCode, body::Incoming};
use tokio::{sync::watch, time::Instant};

use crate::{
    config::{Config, Limits},
    legacy::{BoxError, GatewayBody, LegacyUpstream, full, timed_body, validate_framing},
};

pub(crate) type HandlerFuture<'handler> =
    Pin<Box<dyn Future<Output = Result<Response<GatewayBody>, GatewayFailure>> + Send + 'handler>>;

/// Transport-neutral ownership contract; application handlers never select an upstream.
pub(crate) trait GatewayHandler: Send + Sync {
    fn handle(&self, request: Request<GatewayBody>, context: RequestContext) -> HandlerFuture<'_>;
}

#[derive(Clone)]
pub(crate) struct RequestContext {
    pub request_id: String,
    pub peer: SocketAddr,
    pub deadline: Instant,
    pub shutdown: watch::Receiver<bool>,
}

impl RequestContext {
    pub fn probe(milliseconds: u64) -> Self {
        Self {
            request_id: "readiness".into(),
            peer: SocketAddr::from(([127, 0, 0, 1], 0)),
            deadline: Instant::now() + Limits::duration(milliseconds),
            shutdown: watch::channel(false).1,
        }
    }
}

#[derive(Debug)]
pub(crate) enum GatewayFailure {
    BadRequest,
    LengthRequired,
    Upstream,
    Deadline,
}

/// Marks a local pre-header timeout eligible for a bounded error-response flush.
#[derive(Clone)]
pub(crate) struct DeadlineResponse;

impl GatewayFailure {
    fn response(&self) -> Response<GatewayBody> {
        let (status, message) = match self {
            Self::BadRequest => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::LengthRequired => (StatusCode::LENGTH_REQUIRED, "known_length_required"),
            Self::Upstream => (StatusCode::BAD_GATEWAY, "upstream_unavailable"),
            Self::Deadline => (StatusCode::GATEWAY_TIMEOUT, "upstream_deadline"),
        };
        let mut response = Response::builder()
            .status(status)
            .header("content-type", "text/plain")
            .body(full(message))
            .expect("static response");
        if matches!(self, Self::Deadline) {
            response.extensions_mut().insert(DeadlineResponse);
        }
        response
    }
}

/// A family owns its exact root and descendants for its complete method set.
pub(crate) struct FamilyRegistration {
    pub id: &'static str,
    pub path: &'static str,
    pub methods: Vec<Method>,
    pub handler: Arc<dyn GatewayHandler>,
}

impl FamilyRegistration {
    fn matches(&self, path: &str) -> bool {
        path == self.path
            || path
                .strip_prefix(self.path)
                .is_some_and(|suffix| suffix.starts_with('/'))
    }
}

pub(crate) struct Gateway {
    legacy: Arc<LegacyUpstream>,
    families: Vec<FamilyRegistration>,
    enabled: HashSet<String>,
    trusted_proxies: Vec<IpAddr>,
    limits: Limits,
    sequence: AtomicU64,
    instance: u128,
    shutdown: watch::Receiver<bool>,
}

impl Gateway {
    pub fn new(
        config: &Config,
        legacy: Arc<LegacyUpstream>,
        families: Vec<FamilyRegistration>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<Self, &'static str> {
        validate_registry(&families, &config.enabled_families)?;
        Ok(Self {
            legacy,
            families,
            enabled: config.enabled_families.iter().cloned().collect(),
            trusted_proxies: config.trusted_proxies.clone(),
            limits: config.limits.clone(),
            sequence: AtomicU64::new(1),
            instance: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "clock_invalid")?
                .as_nanos(),
            shutdown,
        })
    }

    pub async fn public(
        &self,
        request: Request<Incoming>,
        peer: SocketAddr,
        deadline: Instant,
    ) -> Result<Response<GatewayBody>, std::convert::Infallible> {
        let started = Instant::now();
        let request_id = format!(
            "{:x}-{:x}",
            self.instance,
            self.sequence.fetch_add(1, Ordering::Relaxed)
        );
        let context = RequestContext {
            request_id,
            peer,
            deadline,
            shutdown: self.shutdown.clone(),
        };
        let method = request.method().clone();
        let mut request = request.map(|body| {
            timed_body(
                body.map_err(|error| Box::new(error) as BoxError)
                    .boxed_unsync(),
                self.limits.body_idle_ms,
                context.deadline,
            )
        });
        let result = validate_framing(request.headers());
        metadata(
            request.headers_mut(),
            &context,
            self.trusted_proxies.contains(&context.peer.ip()),
        );
        let selected_owner = select_family(&self.families, &self.enabled, &request)
            .map_or("legacy", |family| family.id);
        let (owner, result) = match result {
            Ok(()) => {
                dispatch(
                    &self.families,
                    &self.enabled,
                    self.legacy.as_ref(),
                    request,
                    context.clone(),
                )
                .await
            }
            Err(error) => (selected_owner, Err(error)),
        };
        let error_class = match &result {
            Ok(_) => "none",
            Err(GatewayFailure::BadRequest) => "invalid_request",
            Err(GatewayFailure::LengthRequired) => "length_required",
            Err(GatewayFailure::Upstream) => "upstream",
            Err(GatewayFailure::Deadline) => "deadline",
        };
        let mut response = result.unwrap_or_else(|error| error.response());
        response.headers_mut().insert(
            "x-request-id",
            context.request_id.parse().expect("generated ID"),
        );
        tracing::info!(request_id = %context.request_id, owner, method = %method,
            status = response.status().as_u16(), duration_ms = started.elapsed().as_millis() as u64,
            error_class, draining = *context.shutdown.borrow(), "response_headers");
        Ok(response)
    }

    pub async fn management(
        &self,
        request: Request<Incoming>,
    ) -> Result<Response<GatewayBody>, std::convert::Infallible> {
        let (status, body) = match (request.method(), request.uri().path()) {
            (&Method::GET, "/live") => (StatusCode::OK, "live"),
            (&Method::GET, "/ready") if !*self.shutdown.borrow() && self.legacy.ready().await => {
                (StatusCode::OK, "ready")
            }
            (&Method::GET, "/ready") => (StatusCode::SERVICE_UNAVAILABLE, "not_ready"),
            _ => (StatusCode::NOT_FOUND, "not_found"),
        };
        Ok(Response::builder()
            .status(status)
            .body(full(body))
            .expect("static response"))
    }
}

/// The legacy handler is mandatory, including when registrations are inactive.
async fn dispatch<'registry>(
    families: &'registry [FamilyRegistration],
    enabled: &HashSet<String>,
    legacy: &dyn GatewayHandler,
    request: Request<GatewayBody>,
    context: RequestContext,
) -> (
    &'registry str,
    Result<Response<GatewayBody>, GatewayFailure>,
) {
    let (owner, handler): (&str, &dyn GatewayHandler) =
        match select_family(families, enabled, &request) {
            Some(family) => (family.id, family.handler.as_ref()),
            None => ("legacy", legacy),
        };
    let result = tokio::time::timeout_at(context.deadline, handler.handle(request, context))
        .await
        .unwrap_or(Err(GatewayFailure::Deadline));
    (owner, result)
}

fn select_family<'registry>(
    families: &'registry [FamilyRegistration],
    enabled: &HashSet<String>,
    request: &Request<GatewayBody>,
) -> Option<&'registry FamilyRegistration> {
    families.iter().find(|family| {
        enabled.contains(family.id)
            && family.matches(request.uri().path())
            && family.methods.contains(request.method())
    })
}

fn metadata(headers: &mut hyper::HeaderMap, context: &RequestContext, trusted: bool) {
    let forwarded: Vec<_> = headers
        .keys()
        .filter(|name| {
            name.as_str() == "forwarded"
                || name.as_str().starts_with("x-forwarded-")
                || name.as_str() == "x-real-ip"
        })
        .cloned()
        .collect();
    // Retain only the trusted peer's X-Forwarded-For chain; all other metadata is gateway-owned.
    let prior = trusted
        .then(|| headers.get("x-forwarded-for").cloned())
        .flatten();
    for name in forwarded {
        headers.remove(name);
    }
    let value = match prior.and_then(|value| value.to_str().ok().map(str::to_owned)) {
        Some(chain) => format!("{chain}, {}", context.peer.ip()),
        None => context.peer.ip().to_string(),
    };
    headers.insert(
        "x-forwarded-for",
        value.parse().expect("valid header chain"),
    );
    headers.insert("x-forwarded-proto", "https".parse().expect("static header"));
    headers.insert(
        "x-request-id",
        context.request_id.parse().expect("generated ID"),
    );
}

fn validate_registry(
    families: &[FamilyRegistration],
    enabled: &[String],
) -> Result<(), &'static str> {
    let mut ids = HashSet::new();
    for (index, family) in families.iter().enumerate() {
        if family.id.is_empty()
            || family.id == "legacy"
            || !ids.insert(family.id)
            || !family.path.starts_with('/')
            || family.path == "/"
            || family.path.ends_with('/')
            || family.path.contains(['?', '#', '%'])
        {
            return Err("family_invalid");
        }
        // Whole-family activation never permits partial method ownership.
        let required = [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::CONNECT,
            Method::OPTIONS,
            Method::TRACE,
            Method::PATCH,
        ];
        if family.methods.len() != required.len()
            || !required
                .iter()
                .all(|method| family.methods.contains(method))
        {
            return Err("family_methods_incomplete");
        }
        if families[..index]
            .iter()
            .any(|other| other.matches(family.path) || family.matches(other.path))
        {
            return Err("family_overlap");
        }
    }
    let mut selected = HashSet::new();
    for id in enabled {
        if !ids.contains(id.as_str()) || !selected.insert(id) {
            return Err("activation_invalid");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;
    impl GatewayHandler for Stub {
        fn handle(&self, _: Request<GatewayBody>, _: RequestContext) -> HandlerFuture<'_> {
            Box::pin(async { Ok(Response::new(full("slice"))) })
        }
    }
    fn family(id: &'static str, path: &'static str) -> FamilyRegistration {
        FamilyRegistration {
            id,
            path,
            methods: vec![
                Method::GET,
                Method::HEAD,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::CONNECT,
                Method::OPTIONS,
                Method::TRACE,
                Method::PATCH,
            ],
            handler: Arc::new(Stub),
        }
    }

    struct RecordingStub {
        name: &'static str,
        calls: std::sync::Mutex<Vec<(String, String, SocketAddr)>>,
    }

    impl GatewayHandler for RecordingStub {
        fn handle(
            &self,
            request: Request<GatewayBody>,
            context: RequestContext,
        ) -> HandlerFuture<'_> {
            Box::pin(async move {
                assert_eq!(request.method(), Method::POST);
                assert_eq!(request.headers()["host"], "shop.example");
                assert!(context.deadline > Instant::now());
                assert!(!*context.shutdown.borrow());
                let uri = request.uri().to_string();
                assert_eq!(
                    request.into_body().collect().await.unwrap().to_bytes(),
                    "form=bytes"
                );
                self.calls
                    .lock()
                    .unwrap()
                    .push((uri, context.request_id, context.peer));
                // Two distinct frames prove the dispatch seam returns a streaming body.
                let body = TestFrames(VecDeque::from([
                    bytes::Bytes::from_static(self.name.as_bytes()),
                    bytes::Bytes::from_static(b" response"),
                ]))
                .boxed_unsync();
                Ok(Response::builder()
                    .status(StatusCode::CREATED)
                    .header("x-owner", self.name)
                    .body(body)
                    .unwrap())
            })
        }
    }

    use std::collections::VecDeque;
    struct TestFrames(VecDeque<bytes::Bytes>);
    impl http_body::Body for TestFrames {
        type Data = bytes::Bytes;
        type Error = BoxError;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            std::task::Poll::Ready(
                self.0
                    .pop_front()
                    .map(|bytes| Ok(http_body::Frame::data(bytes))),
            )
        }
    }

    #[tokio::test]
    async fn registered_handler_dispatch_and_mandatory_fallback() {
        let slice = Arc::new(RecordingStub {
            name: "slice",
            calls: Default::default(),
        });
        let legacy = RecordingStub {
            name: "legacy",
            calls: Default::default(),
        };
        let mut registration = family("account", "/app/account");
        registration.handler = slice.clone();
        let registry = vec![registration];
        let context = RequestContext::probe(1000);
        for (active, path, expected) in [
            (false, "/app/account/edit?raw=%2F", "legacy"),
            (true, "/app/account/edit?raw=%2F", "slice"),
            (true, "/unknown?raw=%2F", "legacy"),
        ] {
            let enabled = if active {
                HashSet::from(["account".into()])
            } else {
                HashSet::new()
            };
            validate_registry(&registry, &enabled.iter().cloned().collect::<Vec<_>>()).unwrap();
            let request = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("host", "shop.example")
                .body(full("form=bytes"))
                .unwrap();
            let (owner, response) =
                dispatch(&registry, &enabled, &legacy, request, context.clone()).await;
            assert_eq!(
                owner,
                if expected == "slice" {
                    "account"
                } else {
                    "legacy"
                }
            );
            let response = response.unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(response.headers()["x-owner"], expected);
            let mut body = response.into_body();
            assert_eq!(
                body.frame().await.unwrap().unwrap().into_data().unwrap(),
                expected
            );
            assert_eq!(
                body.frame().await.unwrap().unwrap().into_data().unwrap(),
                " response"
            );
            assert!(body.frame().await.is_none());
            let calls = if expected == "slice" {
                &slice.calls
            } else {
                &legacy.calls
            };
            assert_eq!(
                calls.lock().unwrap().last().unwrap(),
                &(path.into(), context.request_id.clone(), context.peer)
            );
        }
        assert_eq!(slice.calls.lock().unwrap().len(), 1);
        assert_eq!(legacy.calls.lock().unwrap().len(), 2);
    }
    #[test]
    fn registration_and_activation_are_separate() {
        let registry = vec![family("account", "/app/account")];
        let request = Request::builder()
            .uri("/app/account/edit")
            .body(full(""))
            .unwrap();
        assert!(select_family(&registry, &HashSet::new(), &request).is_none());
        let enabled = HashSet::from(["account".to_owned()]);
        assert_eq!(
            select_family(&registry, &enabled, &request).unwrap().id,
            "account"
        );
        let unknown = Request::builder().uri("/unknown").body(full("")).unwrap();
        assert!(select_family(&registry, &enabled, &unknown).is_none());
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert!(config.enabled_families.is_empty());
        assert!(validate_registry(&[family("account", "/app/account")], &[]).is_ok());
        assert!(
            validate_registry(&[family("account", "/app/account")], &["account".into()]).is_ok()
        );
        assert!(validate_registry(&[], &["account".into()]).is_err());
        assert!(
            validate_registry(
                &[
                    family("a", "/app/account"),
                    family("b", "/app/account/edit")
                ],
                &[]
            )
            .is_err()
        );
        assert!(validate_registry(&[family("a", "/a"), family("a", "/b")], &[]).is_err());
        let mut incomplete = family("a", "/a");
        incomplete.methods.pop();
        assert!(validate_registry(&[incomplete], &[]).is_err());
        assert!(!family("a", "/app/account").matches("/app/accounts"));
    }
    #[test]
    fn spoofed_metadata_is_removed() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("forwarded", "for=secret".parse().unwrap());
        headers.insert("x-forwarded-for", "spoof".parse().unwrap());
        metadata(&mut headers, &RequestContext::probe(100), false);
        assert!(!headers.contains_key("forwarded"));
        assert_eq!(headers["x-forwarded-for"], "127.0.0.1");
    }
}
