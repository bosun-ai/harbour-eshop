use eshop_gateway::{
    Error,
    config::Config,
    ownership::{Dispatcher, registrations},
    server,
};

#[tokio::main]
async fn main() {
    if start().await.is_err() {
        // Never expose configuration values, PEM material or upstream URLs.
        eprintln!("gateway startup/runtime failure");
        std::process::exit(1);
    }
}

async fn start() -> Result<(), Error> {
    let config = Config::from_env()?;
    let dispatcher = Dispatcher::new(registrations(), &config.enabled_slices)?;
    server::run(config, dispatcher, async {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("signal handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    })
    .await
}
