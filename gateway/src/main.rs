mod config;
mod dispatch;
mod ingress;
mod operations;
mod proxy;

#[tokio::main]
async fn main() {
    let result = match std::env::args().nth(1).as_deref() {
        Some("serve") => match config::Config::load() {
            Ok(config) => {
                tracing_subscriber::fmt()
                    .json()
                    .with_max_level(config.log_level)
                    .init();
                ingress::serve(std::sync::Arc::new(config)).await
            }
            Err(_) => Err("invalid gateway configuration or TLS material".into()),
        },
        Some("check-ready") => operations::check_ready().await,
        _ => Err("usage: eshop-gateway serve | check-ready".into()),
    };
    if result.is_err() {
        eprintln!("gateway failed: configuration, listener, or readiness unavailable");
        std::process::exit(1);
    }
}
