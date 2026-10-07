//! Ownership declarations are independent from implementation registration and activation.
use crate::Body;
use hyper::{Method, Request, Response};
use std::{collections::HashSet, future::Future, pin::Pin, sync::Arc};

pub type HandlerFuture = Pin<Box<dyn Future<Output = Response<Body>> + Send>>;

/// Future slices own their dependencies; failure responses never fall back to Harbour.
pub trait SliceHandler: Send + Sync {
    fn handle(&self, request: Request<Body>) -> HandlerFuture;
}

#[derive(Clone)]
pub enum PathMatcher {
    Exact(String),
    Prefix(String),
}

impl PathMatcher {
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(value) => path == value,
            Self::Prefix(value) => path.starts_with(value),
        }
    }
    fn value(&self) -> &str {
        match self {
            Self::Exact(value) | Self::Prefix(value) => value,
        }
    }
    fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(left), Self::Exact(right)) => left == right,
            (Self::Prefix(left), Self::Prefix(right)) => {
                left.starts_with(right) || right.starts_with(left)
            }
            (Self::Exact(path), prefix) | (prefix, Self::Exact(path)) => prefix.matches(path),
        }
    }
}

/// Nonempty path and method sets declare complete ownership for those selectors.
/// Methods outside the declaration remain legacy-owned.
pub struct FamilyDescriptor {
    pub id: String,
    pub paths: Vec<PathMatcher>,
    pub methods: Vec<Method>,
}

pub struct RegisteredFamily {
    pub descriptor: FamilyDescriptor,
    pub handler: Arc<dyn SliceHandler>,
}

pub struct DispatchTable {
    families: Vec<RegisteredFamily>,
    active: HashSet<String>,
}

impl DispatchTable {
    /// Validate the entire registry, including inactive entries, before activation.
    pub fn build(families: Vec<RegisteredFamily>, active: &[String]) -> Result<Self, &'static str> {
        let mut ids = HashSet::new();
        let mut paths: Vec<&PathMatcher> = Vec::new();
        for family in &families {
            let descriptor = &family.descriptor;
            if descriptor.id.is_empty()
                || !descriptor
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                || !ids.insert(descriptor.id.clone())
                || descriptor.paths.is_empty()
                || descriptor.methods.is_empty()
                || descriptor.methods.iter().collect::<HashSet<_>>().len()
                    != descriptor.methods.len()
            {
                return Err("invalid or incomplete family declaration");
            }
            for path in &descriptor.paths {
                let value = path.value();
                if !value.starts_with('/')
                    || value.contains(['?', '#', '%', '\\'])
                    || value.chars().any(char::is_whitespace)
                    || matches!(path, PathMatcher::Prefix(_) if !value.ends_with('/'))
                {
                    return Err("malformed family selector");
                }
                if paths.iter().any(|other| path.overlaps(other)) {
                    return Err("overlapping families");
                }
                paths.push(path);
            }
        }
        let active_set: HashSet<_> = active.iter().cloned().collect();
        if active_set.len() != active.len() || active_set.iter().any(|id| !ids.contains(id)) {
            return Err("unknown or duplicate activation ID");
        }
        Ok(Self {
            families,
            active: active_set,
        })
    }

    /// Match the untouched URI path; no decoding or normalization is performed.
    pub fn select(&self, method: &Method, path: &str) -> Option<(&str, Arc<dyn SliceHandler>)> {
        self.families
            .iter()
            .find(|family| {
                self.active.contains(&family.descriptor.id)
                    && family.descriptor.methods.contains(method)
                    && family
                        .descriptor
                        .paths
                        .iter()
                        .any(|matcher| matcher.matches(path))
            })
            .map(|family| (family.descriptor.id.as_str(), family.handler.clone()))
    }
}
