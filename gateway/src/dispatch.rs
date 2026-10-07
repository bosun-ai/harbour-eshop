//! Complete-family registration is separate from explicit activation.
use crate::{Body, Error};
use hyper::{Method, Request, Response};
use std::{
    collections::HashSet, future::Future, net::SocketAddr, pin::Pin, sync::Arc, time::Instant,
};

/// Gateway-local metadata; never contains credentials or parsed sessions.
#[derive(Clone)]
pub struct Context {
    pub correlation: u64,
    pub peer: SocketAddr,
    pub started: Instant,
    pub owner: &'static str,
}

/// HTTP-native interface used by every owner.
pub trait Handler: Send + Sync {
    fn handle(
        &self,
        request: Request<Body>,
        context: Context,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send + '_>>;
}

/// A complete family owns whole path subtrees and explicitly enumerated methods.
/// Prefixes include their root and descendants, not similarly named paths.
pub struct Family {
    pub id: &'static str,
    pub paths: Vec<&'static str>,
    pub methods: Vec<Method>,
    pub handler: Arc<dyn Handler>,
}

fn contains(root: &str, path: &str) -> bool {
    root == "/"
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Immutable validated selection table. Unknown paths always select Harbour.
pub struct Dispatch {
    legacy: Arc<dyn Handler>,
    active: Vec<Family>,
}

impl Dispatch {
    /// Validate registrations and activation before accepting connections.
    pub fn new(
        legacy: Arc<dyn Handler>,
        families: Vec<Family>,
        ids: &[String],
    ) -> Result<Self, Error> {
        let mut known = HashSet::new();
        for family in &families {
            if family.id.is_empty()
                || !known.insert(family.id)
                || family.paths.is_empty()
                || family.methods.is_empty()
                || family.paths.iter().any(|path| {
                    !path.starts_with('/')
                        || (path.len() > 1 && path.ends_with('/'))
                        || path.contains(['?', '#', '%'])
                })
            {
                return Err("incomplete or duplicate family registration".into());
            }
        }
        let mut enabled = HashSet::new();
        for id in ids {
            if !known.contains(id.as_str()) || !enabled.insert(id.as_str()) {
                return Err("unknown or duplicate active family".into());
            }
        }
        let active: Vec<_> = families
            .into_iter()
            .filter(|family| enabled.contains(family.id))
            .collect();
        for (index, first) in active.iter().enumerate() {
            for second in &active[index + 1..] {
                if first.paths.iter().any(|a| {
                    second
                        .paths
                        .iter()
                        .any(|b| contains(a, b) || contains(b, a))
                }) {
                    return Err("overlapping active family ownership".into());
                }
            }
        }
        Ok(Self { legacy, active })
    }

    /// Select on the original path without decoding or rewriting the URI.
    pub fn select(&self, path: &str, method: &Method) -> (&'static str, Arc<dyn Handler>) {
        for family in &self.active {
            if family.paths.iter().any(|root| contains(root, path))
                && family.methods.contains(method)
            {
                return (family.id, family.handler.clone());
            }
        }
        ("LegacyUpstream", self.legacy.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Stub;
    impl Handler for Stub {
        fn handle(
            &self,
            _: Request<Body>,
            _: Context,
        ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send + '_>> {
            Box::pin(async { Ok(Response::new(crate::body("test"))) })
        }
    }
    fn family(id: &'static str, path: &'static str) -> Family {
        Family {
            id,
            paths: vec![path],
            methods: vec![Method::GET, Method::POST],
            handler: Arc::new(Stub),
        }
    }
    #[test]
    fn activation_is_explicit_and_atomic() {
        let dispatch =
            Dispatch::new(Arc::new(Stub), vec![family("cart", "/app/cart")], &[]).unwrap();
        assert_eq!(
            dispatch.select("/app/cart", &Method::GET).0,
            "LegacyUpstream"
        );
        let dispatch = Dispatch::new(
            Arc::new(Stub),
            vec![family("cart", "/app/cart")],
            &["cart".into()],
        )
        .unwrap();
        assert_eq!(dispatch.select("/app/cart/edit", &Method::POST).0, "cart");
        for path in ["/unknown", "/app/carts", "/app/%63art"] {
            assert_eq!(dispatch.select(path, &Method::GET).0, "LegacyUpstream");
        }
        assert_eq!(
            dispatch.select("/app/cart", &Method::DELETE).0,
            "LegacyUpstream"
        );
        assert!(Dispatch::new(Arc::new(Stub), vec![], &["missing".into()]).is_err());
        assert!(
            Dispatch::new(
                Arc::new(Stub),
                vec![family("a", "/app"), family("b", "/app/cart")],
                &["a".into(), "b".into()]
            )
            .is_err()
        );
        assert!(Dispatch::new(Arc::new(Stub), vec![family("", "/app")], &[]).is_err());
        assert!(Dispatch::new(Arc::new(Stub), vec![family("a", "relative")], &[]).is_err());
        assert!(
            Dispatch::new(
                Arc::new(Stub),
                vec![family("a", "/app")],
                &["a".into(), "a".into()]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn inactive_handler_does_not_receive_requests() {
        struct Legacy;
        impl Handler for Legacy {
            fn handle(
                &self,
                _: Request<Body>,
                _: Context,
            ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send + '_>>
            {
                Box::pin(async { Ok(Response::new(crate::body("legacy"))) })
            }
        }
        use http_body_util::BodyExt;
        let dispatch =
            Dispatch::new(Arc::new(Legacy), vec![family("cart", "/app/cart")], &[]).unwrap();
        let (owner, handler) = dispatch.select("/app/cart", &Method::GET);
        let response = handler
            .handle(
                Request::new(crate::body("")),
                Context {
                    correlation: 1,
                    peer: "127.0.0.1:1".parse().unwrap(),
                    started: Instant::now(),
                    owner,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "legacy"
        );
    }
}
