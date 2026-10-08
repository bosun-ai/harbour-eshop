//! Opt-in transport boundary; application behavior remains in Harbour.
mod config;
mod dispatch;
mod legacy;
mod server;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{Response, StatusCode};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, Error>;

fn response(status: StatusCode, text: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(
            Full::new(Bytes::from_static(text.as_bytes()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        // Startup errors contain setting names, never setting values or request data.
        eprintln!("gateway startup failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Error> {
    let config = config::Config::from_env()?;
    tracing_subscriber::fmt()
        .json()
        .with_max_level(config.log_level)
        .init();
    let dispatcher = dispatch::Dispatcher::new(vec![], &config.enabled)?;
    let upstream = legacy::LegacyUpstream::new(&config)?;
    server::run(config, dispatcher, upstream).await
}
