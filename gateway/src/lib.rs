//! HTTPS hosting boundary. Application state and behavior remain in Harbour.
pub mod config;
pub mod dispatch;
pub mod legacy;
pub mod runtime;
pub mod slices;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};

/// Streaming body shared by the dispatcher, slices, and upstream adapter.
pub type Body = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Construct a small protocol/management response (never an application page).
pub fn response(status: u16, text: &'static str) -> hyper::Response<Body> {
    hyper::Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(
            Full::new(Bytes::from_static(text.as_bytes()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("constant response")
}
