//! Opt-in transport boundary. Harbour remains the default application owner.
pub mod config;
pub mod legacy;
pub mod ownership;
pub mod server;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::Response;

/// Shared transport error; application handlers need no persistence abstraction.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// Streaming request and response body used by both owners.
pub type Body = UnsyncBoxBody<Bytes, Error>;

/// Construct a small literal body without changing upstream application responses.
pub fn body(text: impl Into<Bytes>) -> Body {
    Full::new(text.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Construct a transport/management response.
pub fn response(status: u16, text: &'static str) -> Response<Body> {
    Response::builder().status(status).body(body(text)).unwrap()
}
