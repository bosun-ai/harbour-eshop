//! Explicit, opt-in gateway entrypoint.
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 || args[1] != "--config" {
        eprintln!("usage: eshop-gateway --config FILE");
        std::process::exit(2);
    }
    if let Err(category) = eshop_gateway::run(&args[2]).await {
        eprintln!("gateway startup/runtime error: {category}");
        std::process::exit(1);
    }
}
