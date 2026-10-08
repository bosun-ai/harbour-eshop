//! Explicit method/path ownership. Compiled handlers are inert until enabled.
use crate::{Body, Error};
use hyper::{Method, Request, Response};
use std::{collections::HashSet, future::Future, pin::Pin};

pub type Handler = fn(Request<Body>) -> Pin<Box<dyn Future<Output = Response<Body>> + Send>>;

pub struct Registration {
    pub id: &'static str,
    pub path: &'static str,
    pub subtree: bool,
    pub methods: Vec<Method>,
    pub handler: Handler,
}

impl Registration {
    fn matches_path(&self, path: &str) -> bool {
        path == self.path
            || (self.subtree
                && (self.path == "/"
                    || path
                        .strip_prefix(self.path)
                        .is_some_and(|rest| rest.starts_with('/'))))
    }
}

pub struct Dispatcher {
    active: Vec<Registration>,
}

impl Dispatcher {
    pub fn new(registrations: Vec<Registration>, enabled: &[String]) -> Result<Self, Error> {
        let mut ids = HashSet::new();
        for registration in &registrations {
            if registration.id.is_empty()
                || !ids.insert(registration.id)
                || !registration.path.starts_with('/')
                || registration.path.contains(['?', '#'])
                || (registration.path != "/" && registration.path.ends_with('/'))
                || registration.methods.is_empty()
            {
                return Err("invalid slice registration".into());
            }
        }
        if enabled.iter().any(|id| !ids.contains(id.as_str())) {
            return Err("unknown enabled slice ID".into());
        }
        let active: Vec<_> = registrations
            .into_iter()
            .filter(|entry| enabled.iter().any(|id| id == entry.id))
            .collect();
        for (index, entry) in active.iter().enumerate() {
            for other in &active[index + 1..] {
                let paths_overlap = entry.path == other.path
                    || (entry.subtree && entry.matches_path(other.path))
                    || (other.subtree && other.matches_path(entry.path));
                if paths_overlap
                    && entry
                        .methods
                        .iter()
                        .any(|method| other.methods.contains(method))
                {
                    return Err("overlapping active slice ownership".into());
                }
            }
        }
        Ok(Self { active })
    }

    pub fn select(&self, method: &Method, path: &str) -> Option<&Registration> {
        self.active
            .iter()
            .find(|entry| entry.methods.contains(method) && entry.matches_path(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn handler(_: Request<Body>) -> Pin<Box<dyn Future<Output = Response<Body>> + Send>> {
        Box::pin(async { crate::response(hyper::StatusCode::OK, "independent") })
    }
    fn entry(id: &'static str, path: &'static str, subtree: bool) -> Registration {
        Registration {
            id,
            path,
            subtree,
            methods: vec![Method::GET],
            handler,
        }
    }
    #[tokio::test]
    async fn activation_boundaries_and_fallback() {
        let disabled = Dispatcher::new(vec![entry("catalogue", "/catalogue", true)], &[]).unwrap();
        assert!(disabled.select(&Method::GET, "/catalogue").is_none());
        let active = Dispatcher::new(
            vec![entry("catalogue", "/catalogue", true)],
            &["catalogue".into()],
        )
        .unwrap();
        for path in ["/catalogue", "/catalogue/items"] {
            assert!(active.select(&Method::GET, path).is_some());
        }
        for path in ["/catalogue-other", "/missing"] {
            assert!(active.select(&Method::GET, path).is_none());
        }
        assert!(active.select(&Method::POST, "/catalogue").is_none());
        let selected = active.select(&Method::GET, "/catalogue").unwrap();
        assert_eq!(
            (selected.handler)(Request::new(
                crate::response(hyper::StatusCode::OK, "").into_body()
            ))
            .await
            .status(),
            200
        );
        assert!(Dispatcher::new(vec![], &["unknown".into()]).is_err());
        assert!(
            Dispatcher::new(
                vec![
                    entry("a", "/catalogue", true),
                    entry("b", "/catalogue/item", false)
                ],
                &["a".into(), "b".into()]
            )
            .is_err()
        );
        assert!(
            Dispatcher::new(
                vec![entry("a", "/", true), entry("b", "/hello", false)],
                &["a".into(), "b".into()]
            )
            .is_err()
        );
        assert!(Dispatcher::new(vec![entry("a", "bad", false)], &[]).is_err());
        assert!(
            Dispatcher::new(vec![entry("a", "/a", false), entry("a", "/b", false)], &[]).is_err()
        );
    }
}
