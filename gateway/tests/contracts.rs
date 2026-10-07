use eshop_gateway::{
    Body,
    config::Config,
    dispatch::{
        DispatchRegistry, FamilySpec, HttpHandler, PathMatcher, RequestContext, TransportFailure,
    },
    legacy::strip_hop_headers,
};
use hyper::{Method, Request, Response};
use std::{future::Future, pin::Pin, sync::Arc};

struct Dummy;
impl HttpHandler for Dummy {
    fn handle(
        &self,
        _: Request<Body>,
        _: RequestContext,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, TransportFailure>> + Send + '_>> {
        Box::pin(async { Err(TransportFailure::Upstream) })
    }
}
fn family() -> FamilySpec {
    FamilySpec {
        id: "account".into(),
        paths: vec![PathMatcher::Prefix("/app/account".into())],
        methods: vec![Method::GET],
        handler: Arc::new(Dummy),
    }
}

#[test]
fn registration_is_not_activation() {
    assert!(PathMatcher::Prefix("/".into()).matches("/unknown"));
    let registry = DispatchRegistry::new(Arc::new(Dummy), vec![family()], &[]).unwrap();
    assert_eq!(registry.select("/app/account", &Method::GET).0, "legacy");
    let registry =
        DispatchRegistry::new(Arc::new(Dummy), vec![family()], &["account".into()]).unwrap();
    assert_eq!(
        registry.select("/app/account/edit", &Method::GET).0,
        "account"
    );
    for path in ["/app/accounting", "/unknown", "/app/account%2fedit"] {
        assert_eq!(registry.select(path, &Method::GET).0, "legacy");
    }
    assert_eq!(registry.select("/app/account", &Method::POST).0, "legacy");
}

#[test]
fn invalid_activation_and_overlap_fail_closed() {
    let mut exact = family();
    exact.paths = vec![
        PathMatcher::Exact("/app/account/edit".into()),
        PathMatcher::Prefix("/app/account".into()),
    ];
    assert!(DispatchRegistry::new(Arc::new(Dummy), vec![exact], &[]).is_err());
    assert!(DispatchRegistry::new(Arc::new(Dummy), vec![], &["missing".into()]).is_err());
    assert!(
        DispatchRegistry::new(
            Arc::new(Dummy),
            vec![family()],
            &["account".into(), "account".into()]
        )
        .is_err()
    );
    let mut duplicate = family();
    duplicate.id = "other".into();
    assert!(DispatchRegistry::new(Arc::new(Dummy), vec![family(), duplicate], &[]).is_err());
    let mut invalid = family();
    invalid.methods.clear();
    assert!(DispatchRegistry::new(Arc::new(Dummy), vec![invalid], &[]).is_err());
    let mut invalid = family();
    invalid.paths.clear();
    assert!(DispatchRegistry::new(Arc::new(Dummy), vec![invalid], &[]).is_err());
    let mut invalid = family();
    invalid.methods.push(Method::GET);
    assert!(DispatchRegistry::new(Arc::new(Dummy), vec![invalid], &[]).is_err());
}

#[test]
fn hop_headers_removed_and_cookies_kept_separate() {
    let mut headers = hyper::HeaderMap::new();
    headers.append("connection", "keep-alive, x-private".parse().unwrap());
    headers.append("connection", "x-other".parse().unwrap());
    for name in ["x-private", "x-other", "transfer-encoding", "upgrade"] {
        headers.insert(name, "value".parse().unwrap());
    }
    headers.append("set-cookie", "one=1".parse().unwrap());
    headers.append("set-cookie", "two=2".parse().unwrap());
    strip_hop_headers(&mut headers);
    assert_eq!(headers.len(), 2);
    assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
}

#[tokio::test]
async fn active_failure_never_falls_back() {
    let registry =
        DispatchRegistry::new(Arc::new(Dummy), vec![family()], &["account".into()]).unwrap();
    let (owner, handler) = registry.select("/app/account", &Method::GET);
    let context = RequestContext {
        correlation_id: "test".into(),
        client_ip: "127.0.0.1".parse().unwrap(),
        deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    assert_eq!(owner, "account");
    assert_eq!(
        handler
            .handle(
                Request::new(eshop_gateway::response(200, "").into_body()),
                context
            )
            .await
            .unwrap_err(),
        TransportFailure::Upstream
    );
}

#[test]
fn config_rejects_unsafe_and_unbounded_settings() {
    let text = include_str!("../config/all-legacy.toml");
    let config: Config = toml::from_str(text).unwrap();
    config.validate().unwrap();
    for uri in [
        "http://legacy:8002/",
        "https://user@legacy:8002/",
        "https://legacy:8002/app",
        "https://legacy:8002/?q=secret",
        "https://legacy:8002/#fragment",
        "https://legacy:443/",
    ] {
        let mut invalid = config.clone();
        invalid.legacy_upstream = uri.into();
        assert!(invalid.validate().is_err());
    }
    let mut invalid = config.clone();
    invalid.management_bind = "0.0.0.0:8003".parse().unwrap();
    assert!(invalid.validate().is_err());
    let mut invalid = config.clone();
    invalid.deadline_ms = 0;
    assert!(invalid.validate().is_err());
    let mut invalid = config;
    invalid.max_connections = 0;
    assert!(invalid.validate().is_err());
    assert!(toml::from_str::<Config>(&format!("{text}\ninsecure = true")).is_err());
}
