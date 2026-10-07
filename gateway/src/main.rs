use eshop_gateway::{config::GatewayConfig, runtime};
use std::path::Path;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.len() != 3 || arguments[1] != "--config" {
        eprintln!("usage: eshop-gateway --config FILE");
        std::process::exit(2);
    }
    let config = match GatewayConfig::load_and_validate(Path::new(&arguments[2])) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("gateway configuration: {error}");
            std::process::exit(2);
        }
    };
    let level = config
        .settings
        .log_level
        .parse::<tracing::Level>()
        .expect("validated log level");
    // Dependency debug/trace output is not part of the secret-free log contract.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(false)
                .with_filter(
                    tracing_subscriber::filter::Targets::new().with_target("eshop_gateway", level),
                ),
        )
        .init();
    if let Err(error) = runtime::run(config).await {
        eprintln!("gateway startup/runtime: {error}");
        std::process::exit(1);
    }
}
