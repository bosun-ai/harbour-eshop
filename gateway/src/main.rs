use eshop_gateway::{
    Error, body,
    config::Config,
    dispatch::{Context, Dispatch, Handler},
    legacy::LegacyUpstream,
    server,
};
use http_body_util::BodyExt;
use hyper::Request;
use std::{sync::Arc, time::Instant};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Error> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args != ["serve"] && args != ["probe", "live"] && args != ["probe", "ready"] {
        return Err("usage: eshop-gateway serve | probe live | probe ready".into());
    }
    let config = Arc::new(Config::load()?);
    let legacy = Arc::new(LegacyUpstream(config.clone()));
    let dispatch = Arc::new(Dispatch::new(legacy.clone(), vec![], &config.active)?);
    tracing_subscriber::fmt()
        .json()
        .with_max_level(config.level)
        .with_target(false)
        .init();
    if args == ["serve"] {
        return server::serve(config, dispatch).await;
    }
    let mut address = config.bind;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    tokio::time::timeout(config.connect, tokio::net::TcpStream::connect(address))
        .await
        .map_err(|_| "live probe deadline")?
        .map_err(|_| "live probe failed")?;
    if args == ["probe", "ready"] {
        let probe = async {
            let request = Request::builder()
                .uri("/hello")
                .header("host", config.upstream.authority().unwrap().as_str())
                .body(body(""))?;
            let response = legacy
                .handle(
                    request,
                    Context {
                        correlation: 0,
                        peer: address,
                        started: Instant::now(),
                        owner: "LegacyUpstream",
                    },
                )
                .await?;
            if response.status() != 200 {
                return Err("ready probe status".into());
            }
            let bytes = response.into_body().collect().await?.to_bytes();
            if bytes != "Hello!" {
                return Err("ready probe content".into());
            }
            Ok::<_, Error>(())
        };
        tokio::time::timeout(config.connect, probe)
            .await
            .map_err(|_| "ready probe deadline")?
            .map_err(|_| "ready probe failed")?;
    }
    Ok(())
}
