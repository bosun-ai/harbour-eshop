// Exercise the binary's private boundary modules without adding a public library API.
#![allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[path = "../src/gateway.rs"]
mod gateway;
#[path = "../src/legacy.rs"]
mod legacy;
#[path = "../src/transport.rs"]
mod transport;
type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, Error>;

use hyper::{Method, Request, StatusCode};
use std::collections::HashSet;

fn handler(
    _: Request<Body>,
    context: gateway::Context,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<hyper::Response<Body>, Error>> + Send>>
{
    Box::pin(async move {
        assert!(context.request_id > 0);
        assert!(context.peer.ip().is_loopback());
        Ok(legacy::text(StatusCode::OK, "independent"))
    })
}

fn entry(id: &'static str, path: &'static str) -> gateway::Registration {
    gateway::Registration {
        id,
        path,
        methods: vec![Method::GET],
        handler,
    }
}

#[tokio::test]
async fn activation_is_separate_from_registration() {
    let entries = vec![entry("catalogue", "/app/shopping")];
    let disabled = HashSet::new();
    gateway::validate(&entries, &disabled).unwrap();
    assert!(gateway::select(&entries, &disabled, "/app/shopping", &Method::GET).is_none());
    let enabled = HashSet::from(["catalogue".to_owned()]);
    gateway::validate(&entries, &enabled).unwrap();
    let selected = gateway::select(&entries, &enabled, "/app/shopping/page", &Method::GET).unwrap();
    let response = (selected.handler)(
        Request::new(legacy::text(StatusCode::OK, "").into_body()),
        gateway::Context {
            request_id: 42,
            peer: "127.0.0.1:1".parse().unwrap(),
        },
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for (path, method) in [
        ("/app/shopping-extra", Method::GET),
        ("/app/shopping", Method::POST),
        ("/hello", Method::GET),
    ] {
        assert!(gateway::select(&entries, &enabled, path, &method).is_none());
    }
    assert!(gateway::validate(&entries, &HashSet::from(["unknown".into()])).is_err());
    assert!(gateway::registrations().is_empty());
}

#[test]
fn invalid_ownership_and_origins() {
    for path in ["/", "relative", "/app/", "/app?x", "/app#x", "/app%2f"] {
        assert!(gateway::validate(&[entry("invalid", path)], &HashSet::new()).is_err());
    }
    for entries in [
        vec![entry("same", "/a"), entry("same", "/b")],
        vec![entry("a", "/a"), entry("b", "/a/b")],
    ] {
        assert!(gateway::validate(&entries, &HashSet::new()).is_err());
    }
    let mut invalid = entry("a", "/a");
    invalid.methods.clear();
    assert!(gateway::validate(&[invalid], &HashSet::new()).is_err());
    for value in [
        "http://legacy:8002",
        "https://user@legacy",
        "https://legacy/base",
        "https://legacy/?x",
        "https://legacy/#x",
    ] {
        assert!(config::origin(value).is_err(), "{value}");
    }
    assert!(config::origin("https://legacy:8002").is_ok());
}

#[test]
fn headers_keep_duplicates_and_remove_connection_nominations() {
    let mut headers = hyper::HeaderMap::new();
    headers.append("set-cookie", "a=1".parse().unwrap());
    headers.append("set-cookie", "b=2".parse().unwrap());
    headers.insert("connection", "x-secret, keep-alive".parse().unwrap());
    headers.insert("x-secret", "secret".parse().unwrap());
    headers.insert("x-forwarded-for", "spoof".parse().unwrap());
    headers.insert("forwarded", "spoof".parse().unwrap());
    headers.insert("host", "public.example".parse().unwrap());
    legacy::strip_hop(&mut headers);
    legacy::strip_untrusted(&mut headers);
    assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
    assert_eq!(headers["host"], "public.example");
    for name in ["connection", "x-secret", "x-forwarded-for", "forwarded"] {
        assert!(!headers.contains_key(name));
    }
}

#[tokio::test]
async fn listener_activation_and_handler_failure_never_fallback() {
    use http_body_util::BodyExt;
    let directory = std::env::temp_dir().join(format!("eshop-gateway-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let cert = directory.join("cert.pem").to_string_lossy().into_owned();
    let key = directory.join("key.pem").to_string_lossy().into_owned();
    assert!(
        std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-keyout",
                &key,
                "-out",
                &cert
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let free = || {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap()
    };
    fn fail(
        _: Request<Body>,
        _: gateway::Context,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<hyper::Response<Body>, Error>> + Send>,
    > {
        Box::pin(async { Err("synthetic failure".into()) })
    }
    for active in [false, true] {
        let public = free();
        let admin = free();
        let config = config::Config {
            public_bind: public,
            admin_bind: admin,
            cert: cert.clone(),
            key: key.clone(),
            origin: config::origin(&format!("https://localhost:{}", free().port())).unwrap(),
            ca: cert.clone(),
            enabled: if active {
                HashSet::from(["shopping".into(), "account".into()])
            } else {
                HashSet::new()
            },
            connect: std::time::Duration::from_secs(1),
            upload: std::time::Duration::from_secs(2),
            response: std::time::Duration::from_secs(3),
            drain: std::time::Duration::from_secs(1),
            log_level: tracing::Level::INFO,
        };
        let entries = vec![
            entry("shopping", "/app/shopping"),
            gateway::Registration {
                id: "account",
                path: "/app/account",
                methods: vec![Method::GET],
                handler: fail,
            },
        ];
        gateway::validate(&entries, &config.enabled).unwrap();
        let mut client_config = config::Config {
            origin: config::origin(&format!("https://localhost:{}", public.port())).unwrap(),
            ..config
        };
        let client = legacy::Legacy::new(&client_config).unwrap();
        client_config.origin =
            config::origin(&format!("https://localhost:{}", free().port())).unwrap();
        let task = tokio::spawn(transport::serve(client_config, entries));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if tokio::net::TcpStream::connect(admin).await.is_ok() {
                break;
            }
            assert!(!task.is_finished() && tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        for (path, expected) in [
            ("/app/shopping", if active { 200 } else { 502 }),
            ("/app/account", if active { 500 } else { 502 }),
            ("/hello", 502),
        ] {
            let request = Request::builder()
                .uri(path)
                .header("host", "localhost")
                .body(legacy::text(StatusCode::OK, "").into_body())
                .unwrap();
            let response = client.forward(request).await.unwrap();
            assert_eq!(response.status().as_u16(), expected);
            response.into_body().collect().await.unwrap();
        }
        task.abort();
        let _ = task.await;
    }
}
