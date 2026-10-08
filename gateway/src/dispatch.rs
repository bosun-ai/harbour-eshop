use std::{collections::HashSet, future::Future, net::SocketAddr, pin::Pin, sync::Arc};

use hyper::{Method, Request, Response};

use crate::legacy::{Body, TransportError};

pub struct Context {
    pub peer: SocketAddr,
    pub correlation_id: u64,
}

pub type HandlerResult =
    Pin<Box<dyn Future<Output = Result<Response<Body>, TransportError>> + Send>>;

/// Shared transport-neutral handler contract; domain handlers need no listener changes.
pub trait Handler: Send + Sync {
    fn handle(&self, request: Request<Body>, context: Context) -> HandlerResult;
}

// These ownership variants are intentionally unused until an application slice is assigned.
#[allow(dead_code)]
pub enum Path {
    Exact(&'static str),
    Family(&'static str),
}

impl Path {
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(base) => path == *base,
            Self::Family(base) => {
                *base == "/"
                    || path == *base
                    || path
                        .strip_prefix(base)
                        .is_some_and(|tail| tail.starts_with('/'))
            }
        }
    }

    fn base(&self) -> &str {
        match self {
            Self::Exact(base) | Self::Family(base) => base,
        }
    }
}

/// Allowed methods are exhaustive; other methods receive 405 from the owner.
pub struct Registration {
    pub id: &'static str,
    pub path: Path,
    pub methods: Vec<Method>,
    pub handler: Arc<dyn Handler>,
}

pub struct Dispatch {
    registrations: Vec<Registration>,
    enabled: HashSet<String>,
    legacy: Arc<dyn Handler>,
}

/// The sole registration point. Bootstrap intentionally has no application owners.
pub fn registrations() -> Vec<Registration> {
    Vec::new()
}

impl Dispatch {
    pub fn new(
        registrations: Vec<Registration>,
        enabled: HashSet<String>,
        legacy: Arc<dyn Handler>,
    ) -> Result<Self, String> {
        let mut ids = HashSet::new();
        for (index, registration) in registrations.iter().enumerate() {
            let base = registration.path.base();
            if registration.id.is_empty()
                || !ids.insert(registration.id)
                || !base.starts_with('/')
                || base.contains(['?', '#'])
                || (base.len() > 1 && base.ends_with('/'))
                || registration.methods.is_empty()
                || registration.methods.iter().collect::<HashSet<_>>().len()
                    != registration.methods.len()
            {
                return Err("dispatch: invalid or incomplete registration".into());
            }
            for other in &registrations[..index] {
                if registration.path.matches(other.path.base()) || other.path.matches(base) {
                    return Err("dispatch: overlapping ownership".into());
                }
            }
        }
        if enabled.iter().any(|id| !ids.contains(id.as_str())) {
            return Err("ENABLED_OWNERS: unknown handler ID".into());
        }
        Ok(Self {
            registrations,
            enabled,
            legacy,
        })
    }

    pub fn select(
        &self,
        path: &str,
        method: &Method,
    ) -> (&str, Option<Arc<dyn Handler>>, Vec<Method>) {
        for registration in &self.registrations {
            if self.enabled.contains(registration.id) && registration.path.matches(path) {
                return (
                    registration.id,
                    registration
                        .methods
                        .contains(method)
                        .then(|| registration.handler.clone()),
                    registration.methods.clone(),
                );
            }
        }
        ("legacy", Some(self.legacy.clone()), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Stub;
    impl Handler for Stub {
        fn handle(&self, _: Request<Body>, _: Context) -> HandlerResult {
            unreachable!()
        }
    }
    fn registration(id: &'static str, path: Path) -> Registration {
        Registration {
            id,
            path,
            methods: vec![Method::GET],
            handler: Arc::new(Stub),
        }
    }
    #[test]
    fn activation_and_method_ownership() {
        let disabled = Dispatch::new(
            vec![registration("cart", Path::Family("/app/cart"))],
            HashSet::new(),
            Arc::new(Stub),
        )
        .unwrap();
        assert_eq!(disabled.select("/app/cart", &Method::GET).0, "legacy");
        let enabled = Dispatch::new(
            vec![registration("cart", Path::Family("/app/cart"))],
            HashSet::from(["cart".into()]),
            Arc::new(Stub),
        )
        .unwrap();
        assert_eq!(enabled.select("/app/cart/edit", &Method::GET).0, "cart");
        assert_eq!(enabled.select("/app/cartoon", &Method::GET).0, "legacy");
        assert!(enabled.select("/app/cart", &Method::POST).1.is_none());
        assert!(Dispatch::new(vec![], HashSet::from(["unknown".into()]), Arc::new(Stub)).is_err());
    }
    #[test]
    fn reject_duplicates_overlap_and_incomplete_policy() {
        for registrations in [
            vec![
                registration("cart", Path::Exact("/a")),
                registration("cart", Path::Exact("/b")),
            ],
            vec![
                registration("cart", Path::Family("/app/cart")),
                registration("edit", Path::Exact("/app/cart/edit")),
            ],
        ] {
            assert!(Dispatch::new(registrations, HashSet::new(), Arc::new(Stub)).is_err());
        }
        let mut incomplete = registration("cart", Path::Exact("/app/cart"));
        incomplete.methods.clear();
        assert!(Dispatch::new(vec![incomplete], HashSet::new(), Arc::new(Stub)).is_err());
    }
}
