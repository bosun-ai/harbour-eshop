use std::{collections::HashSet, sync::Arc};

use harbour_eshop_gateway::dispatch::{
    CompleteMatcher, Dispatch, Dispatcher, PathFamily, SliceRegistration,
};
use hyper::Method;

fn registration(id: &'static str, path: PathFamily, methods: &[Method]) -> SliceRegistration {
    struct Handler;
    impl harbour_eshop_gateway::dispatch::SliceHandler for Handler {
        fn handle(
            &self,
            _: hyper::Request<hyper::body::Incoming>,
        ) -> harbour_eshop_gateway::dispatch::HandlerFuture {
            Box::pin(async { unreachable!() })
        }
    }
    SliceRegistration {
        id,
        complete_matcher: CompleteMatcher {
            path_family: path,
            methods: methods.iter().cloned().collect::<HashSet<_>>(),
        },
        handler: Arc::new(Handler),
    }
}
fn request(method: Method, path: &str) -> hyper::Request<http_body_util::Empty<bytes::Bytes>> {
    hyper::Request::builder()
        .method(method)
        .uri(path)
        .body(http_body_util::Empty::new())
        .unwrap()
}

#[test]
fn inactive_registration_and_unknown_path_fall_through() {
    let dispatcher = Dispatcher::new(
        vec![registration(
            "catalogue",
            PathFamily::Prefix("/app/shopping".into()),
            &[Method::GET],
        )],
        &[],
    )
    .unwrap();
    assert!(matches!(
        dispatcher.dispatch(&request(Method::GET, "/app/shopping")),
        Dispatch::LegacyUpstream
    ));
    assert!(matches!(
        dispatcher.dispatch(&request(Method::GET, "/hello")),
        Dispatch::LegacyUpstream
    ));
}
#[test]
fn active_registration_requires_declared_family_and_method() {
    let dispatcher = Dispatcher::new(
        vec![registration(
            "catalogue",
            PathFamily::Prefix("/app/shopping".into()),
            &[Method::GET, Method::POST],
        )],
        &["catalogue".into()],
    )
    .unwrap();
    assert!(matches!(
        dispatcher.dispatch(&request(Method::GET, "/app/shopping")),
        Dispatch::Slice(_)
    ));
    assert!(matches!(
        dispatcher.dispatch(&request(Method::POST, "/app/shopping/item")),
        Dispatch::Slice(_)
    ));
    assert!(matches!(
        dispatcher.dispatch(&request(Method::DELETE, "/app/shopping")),
        Dispatch::LegacyUpstream
    ));
    assert!(matches!(
        dispatcher.dispatch(&request(Method::GET, "/app/shopping-old")),
        Dispatch::LegacyUpstream
    ));
}
#[test]
fn invalid_activation_and_overlap_fail_startup() {
    assert!(Dispatcher::new(vec![], &["unknown".into()]).is_err());
    let registrations = vec![
        registration("one", PathFamily::Prefix("/app".into()), &[Method::GET]),
        registration(
            "two",
            PathFamily::Exact("/app/login".into()),
            &[Method::GET],
        ),
    ];
    assert!(Dispatcher::new(registrations, &["one".into(), "two".into()]).is_err());
}
