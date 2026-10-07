//! Pure ownership selection; no transport, persistence, session or domain logic.
use crate::Body;
use hyper::{Method, Request, Response};
use std::{collections::HashSet, future::Future, net::IpAddr, pin::Pin, sync::Arc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Gateway-owned metadata, independent of Harbour's opaque session cookies.
#[derive(Clone)]
pub struct RequestContext {
    pub correlation_id: String,
    pub client_ip: IpAddr,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
}

/// Failure categories are safe to log; never contain request or TLS secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    Upstream,
    Deadline,
    Framing,
    Cancelled,
}
impl std::fmt::Display for TransportFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for TransportFailure {}

/// Object-safe asynchronous handler contract for legacy and future slices.
pub trait HttpHandler: Send + Sync {
    fn handle(
        &self,
        request: Request<Body>,
        context: RequestContext,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, TransportFailure>> + Send + '_>>;
}

/// Matching is on raw paths, never query parameters or decoded path aliases.
#[derive(Clone)]
pub enum PathMatcher {
    Exact(String),
    Prefix(String),
}
impl PathMatcher {
    fn path(&self) -> &str {
        match self {
            Self::Exact(path) | Self::Prefix(path) => path,
        }
    }
    /// Prefixes include their root and only slash-delimited descendants.
    pub fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(exact) => path == exact,
            Self::Prefix(prefix) => {
                (prefix == "/" && path.starts_with('/'))
                    || path == prefix
                    || path
                        .strip_prefix(prefix)
                        .is_some_and(|tail| tail.starts_with('/'))
            }
        }
    }
    fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(left), Self::Exact(right)) => left == right,
            (Self::Prefix(_), _) => self.matches(other.path()),
            (_, Self::Prefix(_)) => other.matches(self.path()),
        }
    }
}

/// Complete declaration of a family's explicit method/path ownership.
pub struct FamilySpec {
    pub id: String,
    pub paths: Vec<PathMatcher>,
    pub methods: Vec<Method>,
    pub handler: Arc<dyn HttpHandler>,
}

/// Available declarations do not receive traffic until explicitly enabled.
pub struct DispatchRegistry {
    legacy: Arc<dyn HttpHandler>,
    active: Vec<FamilySpec>,
}
impl DispatchRegistry {
    /// Reject invalid declarations, overlaps and activation before listening.
    pub fn new(
        legacy: Arc<dyn HttpHandler>,
        available: Vec<FamilySpec>,
        enabled: &[String],
    ) -> Result<Self, &'static str> {
        let mut ids = HashSet::new();
        for family in &available {
            if family.id.is_empty()
                || family.id == "legacy"
                || !family
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                || !ids.insert(family.id.clone())
                || family.paths.is_empty()
                || family.methods.is_empty()
                || family.methods.iter().collect::<HashSet<_>>().len() != family.methods.len()
                || family.paths.iter().any(|path| {
                    !path.path().starts_with('/')
                        || path.path().contains(['?', '#', '%'])
                        || (path.path() != "/" && path.path().ends_with('/'))
                })
            {
                return Err("family_declaration");
            }
            for (index, path) in family.paths.iter().enumerate() {
                if family.paths[index + 1..]
                    .iter()
                    .any(|other| path.overlaps(other) || other.overlaps(path))
                {
                    return Err("family_overlap");
                }
            }
        }
        for (index, family) in available.iter().enumerate() {
            for other in &available[index + 1..] {
                if family
                    .methods
                    .iter()
                    .any(|method| other.methods.contains(method))
                    && family.paths.iter().any(|path| {
                        other
                            .paths
                            .iter()
                            .any(|right| path.overlaps(right) || right.overlaps(path))
                    })
                {
                    return Err("family_overlap");
                }
            }
        }
        if enabled.iter().collect::<HashSet<_>>().len() != enabled.len()
            || enabled.iter().any(|id| !ids.contains(id))
        {
            return Err("family_activation");
        }
        Ok(Self {
            legacy,
            active: available
                .into_iter()
                .filter(|family| enabled.contains(&family.id))
                .collect(),
        })
    }

    /// Unmatched requests always remain legacy; handler errors never fall back.
    pub fn select(&self, path: &str, method: &Method) -> (&str, &dyn HttpHandler) {
        for family in &self.active {
            if family.methods.contains(method)
                && family.paths.iter().any(|matcher| matcher.matches(path))
            {
                return (&family.id, family.handler.as_ref());
            }
        }
        ("legacy", self.legacy.as_ref())
    }
}
