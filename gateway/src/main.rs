mod config;
mod gateway;
mod legacy;
mod transport;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, Error>;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("gateway startup/check failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Error> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return transport::healthcheck().await;
    }
    let config = config::Config::load()?;
    tracing_subscriber::fmt()
        .json()
        .with_max_level(config.log_level)
        .with_target(false)
        .init();
    let registrations = gateway::registrations();
    gateway::validate(&registrations, &config.enabled)?;
    transport::serve(config, registrations).await
}
