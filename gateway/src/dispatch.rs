use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use hyper::{body::Incoming, Method, Request, Response};
use thiserror::Error;

use crate::proxy::GatewayBody;

pub type HandlerFuture = Pin<Box<dyn Future<Output = Response<GatewayBody>> + Send>>;

pub trait SliceHandler: Send + Sync {
    fn handle(&self, request: Request<Incoming>) -> HandlerFuture;
}

#[derive(Clone)]
pub enum PathFamily {
    Exact(String),
    Prefix(String),
}

#[derive(Clone)]
pub struct CompleteMatcher {
    pub path_family: PathFamily,
    pub methods: HashSet<Method>,
}

impl CompleteMatcher {
    pub fn matches<Body>(&self, request: &Request<Body>) -> bool {
        self.methods.contains(request.method())
            && match &self.path_family {
                PathFamily::Exact(path) => request.uri().path() == path,
                PathFamily::Prefix(prefix) => {
                    request.uri().path() == prefix
                        || request
                            .uri()
                            .path()
                            .strip_prefix(prefix)
                            .is_some_and(|rest| rest.starts_with('/'))
                }
            }
    }
    fn overlaps(&self, other: &Self) -> bool {
        if self.methods.is_disjoint(&other.methods) {
            return false;
        }
        match (&self.path_family, &other.path_family) {
            (PathFamily::Exact(a), PathFamily::Exact(b)) => a == b,
            (PathFamily::Exact(path), PathFamily::Prefix(prefix))
            | (PathFamily::Prefix(prefix), PathFamily::Exact(path)) => {
                path == prefix
                    || path
                        .strip_prefix(prefix)
                        .is_some_and(|rest| rest.starts_with('/'))
            }
            (PathFamily::Prefix(a), PathFamily::Prefix(b)) => {
                a == b
                    || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
                    || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
            }
        }
    }
}

pub struct SliceRegistration {
    pub id: &'static str,
    pub complete_matcher: CompleteMatcher,
    pub handler: Arc<dyn SliceHandler>,
}
pub enum Dispatch {
    Slice(Arc<dyn SliceHandler>),
    LegacyUpstream,
}

pub struct Dispatcher {
    active: Vec<SliceRegistration>,
}

#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("unknown enabled slice {0}")]
    UnknownSlice(String),
    #[error("slice {0} is registered more than once")]
    DuplicateRegistration(String),
    #[error("active slices {first} and {second} have overlapping path and method claims")]
    Overlap { first: String, second: String },
}

impl Dispatcher {
    pub fn new(
        registrations: Vec<SliceRegistration>,
        enabled: &[String],
    ) -> Result<Self, DispatchError> {
        let mut available = HashMap::new();
        for registration in registrations {
            let id = registration.id;
            if available.insert(id, registration).is_some() {
                return Err(DispatchError::DuplicateRegistration(id.into()));
            }
        }
        let mut active = Vec::new();
        for id in enabled {
            let registration = available
                .remove(id.as_str())
                .ok_or_else(|| DispatchError::UnknownSlice(id.clone()))?;
            if active.iter().any(|other: &SliceRegistration| {
                other
                    .complete_matcher
                    .overlaps(&registration.complete_matcher)
            }) {
                let first = active
                    .iter()
                    .find(|other| {
                        other
                            .complete_matcher
                            .overlaps(&registration.complete_matcher)
                    })
                    .expect("overlap was found");
                return Err(DispatchError::Overlap {
                    first: first.id.into(),
                    second: registration.id.into(),
                });
            }
            active.push(registration);
        }
        Ok(Self { active })
    }
    pub fn dispatch<Body>(&self, request: &Request<Body>) -> Dispatch {
        self.active
            .iter()
            .find(|registration| registration.complete_matcher.matches(request))
            .map(|registration| Dispatch::Slice(Arc::clone(&registration.handler)))
            .unwrap_or(Dispatch::LegacyUpstream)
    }
}

/// Bootstrap has no migrated route family; availability never activates one.
pub fn bootstrap_registrations() -> Vec<SliceRegistration> {
    Vec::new()
}
