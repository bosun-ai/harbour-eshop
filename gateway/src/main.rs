mod config;
mod dispatch;
mod legacy;
mod server;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("gateway: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = std::sync::Arc::new(config::Config::load()?);
    tracing_subscriber::fmt()
        .json()
        .with_max_level(config.log_level)
        .init();
    let legacy = std::sync::Arc::new(legacy::LegacyUpstream::new(config.clone()));
    let dispatch = std::sync::Arc::new(dispatch::Dispatch::new(
        dispatch::registrations(),
        config.enabled.clone(),
        legacy.clone(),
    )?);
    server::run(config, dispatch, legacy).await
}
