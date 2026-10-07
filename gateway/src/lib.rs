//! Opt-in HTTP boundary; Harbour remains the default owner of every request.
pub mod config;
pub mod dispatch;
pub mod legacy;
pub mod server;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};

/// Shared streaming body for legacy and future family handlers.
pub type Body = UnsyncBoxBody<Bytes, Error>;
/// Internal errors are never rendered or logged: they may contain private data.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// A small fixed response body.
pub fn body(text: &'static str) -> Body {
    Full::new(Bytes::from_static(text.as_bytes()))
        .map_err(|never| match never {})
        .boxed_unsync()
}
