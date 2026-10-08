//! Ownership is independent of activation and never method-dependent.
use crate::{Body, Error};
use hyper::{Request, Response};
use std::{collections::HashSet, future::Future, pin::Pin};

/// Gateway-local correlation, never injected into legacy requests.
#[derive(Clone, Copy)]
pub struct Context {
    pub request_id: u64,
}
/// Common asynchronous transport contract for legacy and future slices.
pub type Reply = Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send>>;
/// A slice owns the entire method policy for its paths.
pub type Handler = fn(Request<Body>, Context) -> Reply;

/// Literal path or segment-bounded family. Matching uses the raw URI path.
#[derive(Clone)]
pub enum Path {
    Exact(&'static str),
    Family(&'static str),
}
impl Path {
    fn root(&self) -> &str {
        match self {
            Self::Exact(path) | Self::Family(path) => path,
        }
    }
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(root) => path == *root,
            Self::Family(root) => {
                path == *root
                    || path
                        .strip_prefix(root)
                        .is_some_and(|tail| tail.starts_with('/'))
            }
        }
    }
}
/// One registration per implementation; registration alone cannot activate it.
pub struct Registration {
    pub id: &'static str,
    pub paths: Vec<Path>,
    pub handler: Handler,
}
/// Validated registry and independent activation allow-list.
pub struct Dispatcher {
    registrations: Vec<Registration>,
    enabled: HashSet<String>,
}
impl Dispatcher {
    /// Reject incomplete registrations, duplicate IDs and all ownership overlaps.
    pub fn new(registrations: Vec<Registration>, enabled: &[String]) -> Result<Self, Error> {
        let mut ids = HashSet::new();
        let mut paths: Vec<&Path> = Vec::new();
        for registration in &registrations {
            if registration.id.is_empty()
                || !registration
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                || !ids.insert(registration.id)
                || registration.paths.is_empty()
            {
                return Err("incomplete or duplicate ownership registration".into());
            }
            for path in &registration.paths {
                let root = path.root();
                if !root.starts_with('/')
                    || root.contains(['%', '?', '#'])
                    || root.contains("//")
                    || (matches!(path, Path::Family(_)) && root.ends_with('/'))
                {
                    return Err("invalid ownership path".into());
                }
                if paths
                    .iter()
                    .any(|other| other.matches(root) || path.matches(other.root()))
                {
                    return Err("overlapping ownership".into());
                }
                paths.push(path);
            }
        }
        let mut selected = HashSet::new();
        for id in enabled {
            if !ids.contains(id.as_str()) || !selected.insert(id.clone()) {
                return Err("unknown or duplicate enabled slice".into());
            }
        }
        Ok(Self {
            registrations,
            enabled: selected,
        })
    }
    /// Select by raw path only; absent selection means legacy fallback.
    pub fn select(&self, path: &str) -> Option<&Registration> {
        self.registrations.iter().find(|registration| {
            self.enabled.contains(registration.id)
                && registration.paths.iter().any(|owned| owned.matches(path))
        })
    }
}

/// Bootstrap intentionally ships no migrated application behavior.
pub fn registrations() -> Vec<Registration> {
    vec![]
}
