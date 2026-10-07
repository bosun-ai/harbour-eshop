use eshop_gateway::{dispatch::*, legacy::strip_hop_headers, response};
use hyper::{HeaderMap, Method};
use std::sync::Arc;

struct Stub;
impl SliceHandler for Stub {
    fn handle(&self, _: hyper::Request<eshop_gateway::Body>) -> HandlerFuture {
        Box::pin(async { response(503, "slice failed") })
    }
}
fn family(id: &str, paths: Vec<PathMatcher>) -> RegisteredFamily {
    RegisteredFamily {
        descriptor: FamilyDescriptor {
            id: id.into(),
            paths,
            methods: vec![Method::GET],
        },
        handler: Arc::new(Stub),
    }
}

#[tokio::test]
async fn installation_is_not_activation_and_failure_is_terminal() {
    let paths = || vec![PathMatcher::Exact("/app/cart".into())];
    let inactive = DispatchTable::build(vec![family("cart", paths())], &[]).unwrap();
    assert!(inactive.select(&Method::GET, "/app/cart").is_none());
    let active = DispatchTable::build(vec![family("cart", paths())], &["cart".into()]).unwrap();
    assert!(active.select(&Method::GET, "/app/%63art").is_none());
    assert!(active.select(&Method::POST, "/app/cart").is_none());
    let (_, handler) = active.select(&Method::GET, "/app/cart").unwrap();
    assert_eq!(
        handler
            .handle(hyper::Request::new(response(200, "").into_body()))
            .await
            .status(),
        503
    );
}

#[test]
fn invalid_ownership_is_rejected_even_when_inactive() {
    assert!(DispatchTable::build(vec![], &["cart".into()]).is_err());
    assert!(DispatchTable::build(vec![family("cart", vec![])], &[]).is_err());
    assert!(
        DispatchTable::build(
            vec![family("Bad ID", vec![PathMatcher::Exact("/cart".into())])],
            &[]
        )
        .is_err()
    );
    for path in [
        "cart",
        "/cart?x",
        "/%63art",
        "/cart#x",
        "/cart space",
        "/cart\\x",
    ] {
        assert!(
            DispatchTable::build(
                vec![family("cart", vec![PathMatcher::Exact(path.into())])],
                &[]
            )
            .is_err()
        );
    }
    assert!(
        DispatchTable::build(
            vec![family("cart", vec![PathMatcher::Prefix("/app".into())])],
            &[]
        )
        .is_err()
    );
    assert!(
        DispatchTable::build(
            vec![
                family("app", vec![PathMatcher::Prefix("/app/".into())]),
                family("cart", vec![PathMatcher::Exact("/app/cart".into())])
            ],
            &[]
        )
        .is_err()
    );
    assert!(
        DispatchTable::build(
            vec![family("cart", vec![PathMatcher::Exact("/cart".into())])],
            &["cart".into(), "cart".into()]
        )
        .is_err()
    );
    let mut incomplete = family("cart", vec![PathMatcher::Exact("/cart".into())]);
    incomplete.descriptor.methods.clear();
    assert!(DispatchTable::build(vec![incomplete], &[]).is_err());
}

#[test]
fn strips_all_connection_fields_and_preserves_repeated_end_to_end_headers() {
    let mut headers = HeaderMap::new();
    headers.append("connection", "close, x-secret".parse().unwrap());
    headers.append("connection", "x-other".parse().unwrap());
    for name in [
        "x-secret",
        "x-other",
        "keep-alive",
        "proxy-authorization",
        "transfer-encoding",
        "upgrade",
        "te",
        "trailer",
    ] {
        headers.insert(name, "value".parse().unwrap());
    }
    headers.append("set-cookie", "a=1".parse().unwrap());
    headers.append("set-cookie", "b=2".parse().unwrap());
    strip_hop_headers(&mut headers);
    assert_eq!(headers.len(), 2);
    assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
}
