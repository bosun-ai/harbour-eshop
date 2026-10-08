//! Ownership declarations live here, independently of transport and activation.
use crate::{Body, Error};
use hyper::{Method, Request, Response};
use std::{collections::HashSet, future::Future, net::SocketAddr, pin::Pin};

pub(crate) struct Context {
    #[allow(dead_code)] // Consumed by future independent handlers, not the legacy adapter.
    pub request_id: u64,
    #[allow(dead_code)]
    pub peer: SocketAddr,
}

pub(crate) type Handler = fn(
    Request<Body>,
    Context,
)
    -> Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send>>;

pub(crate) struct Registration {
    pub id: &'static str,
    pub path: &'static str,
    pub methods: Vec<Method>,
    pub handler: Handler,
}

pub(crate) fn registrations() -> Vec<Registration> {
    // Add a handler module and its registration here; merging never enables it.
    Vec::new()
}

fn family(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn validate(entries: &[Registration], enabled: &HashSet<String>) -> Result<(), Error> {
    let mut ids = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.id.is_empty()
            || !ids.insert(entry.id)
            || !entry.path.starts_with('/')
            || entry.path == "/"
            || entry.path.ends_with('/')
            || entry.path.contains(['?', '#', '%'])
            || entry.path.contains("//")
            || entry
                .path
                .split('/')
                .any(|segment| matches!(segment, "." | ".."))
            || !entry
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || entry.methods.is_empty()
            || entry.methods.iter().collect::<HashSet<_>>().len() != entry.methods.len()
        {
            return Err("invalid ownership declaration".into());
        }
        for previous in &entries[..index] {
            if family(entry.path, previous.path) || family(previous.path, entry.path) {
                return Err("overlapping route families".into());
            }
        }
    }
    if enabled.iter().any(|id| !ids.contains(id.as_str())) {
        return Err("unknown enabled route family".into());
    }
    Ok(())
}

pub(crate) fn select<'a>(
    entries: &'a [Registration],
    enabled: &HashSet<String>,
    path: &str,
    method: &Method,
) -> Option<&'a Registration> {
    entries.iter().find(|entry| {
        enabled.contains(entry.id) && family(path, entry.path) && entry.methods.contains(method)
    })
}
