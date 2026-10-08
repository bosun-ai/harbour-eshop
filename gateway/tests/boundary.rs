use eshop_gateway::{
    Body, Error, body,
    config::{Config, origin},
    legacy::{Failure, Legacy, TimedBody, strip_hop_headers},
    ownership::{Context, Dispatcher, Path, Registration, Reply},
    response,
    server::dispatch,
};
use http_body_util::BodyExt;
use hyper::{HeaderMap, Request, Response, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{
    fs,
    io::BufReader,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ClientConfig, RootCertStore, ServerConfig},
};

fn hello(request: Request<Body>, context: Context) -> Reply {
    Box::pin(async move {
        assert!(context.request_id > 0);
        Ok(response(
            if request.method() == "GET" { 200 } else { 405 },
            "slice hello",
        ))
    })
}
fn info(_: Request<Body>, _: Context) -> Reply {
    Box::pin(async { Ok(response(200, "slice info")) })
}
fn registry() -> Vec<Registration> {
    vec![
        Registration {
            id: "hello",
            paths: vec![Path::Exact("/hello")],
            handler: hello,
        },
        Registration {
            id: "info",
            paths: vec![Path::Family("/info")],
            handler: info,
        },
    ]
}

fn fixture() -> Config {
    let (config, directory) = fixture_files();
    fs::remove_dir_all(directory).unwrap();
    config
}

fn fixture_files() -> (Config, std::path::PathBuf) {
    let directory = std::env::temp_dir().join(format!(
        "eshop-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    assert!(
        Command::new("openssl")
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
                "-keyout"
            ])
            .arg(directory.join("key.pem"))
            .arg("-out")
            .arg(directory.join("cert.pem"))
            .output()
            .unwrap()
            .status
            .success()
    );
    let certs = rustls_pemfile::certs(&mut BufReader::new(
        fs::File::open(directory.join("cert.pem")).unwrap(),
    ))
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let key = rustls_pemfile::private_key(&mut BufReader::new(
        fs::File::open(directory.join("key.pem")).unwrap(),
    ))
    .unwrap()
    .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(certs[0].clone()).unwrap();
    let server_tls = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap(),
    );
    let client_tls = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    (
        Config {
            public_bind: "127.0.0.1:0".parse().unwrap(),
            management_bind: "127.0.0.1:0".parse().unwrap(),
            upstream: origin("https://localhost:1").unwrap(),
            server_tls,
            client_tls,
            enabled_slices: vec![],
            connect: Duration::from_secs(1),
            header: Duration::from_secs(1),
            request_body: Duration::from_secs(1),
            upstream_response: Duration::from_secs(1),
            body_idle: Duration::from_secs(1),
            shutdown: Duration::from_secs(1),
            log_level: "off".into(),
        },
        directory,
    )
}
static NEXT: AtomicUsize = AtomicUsize::new(0);

async fn upstream(
    config: &mut Config,
    handler: fn(Request<Body>, Context) -> Reply,
) -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.upstream = origin(&format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let acceptor = TlsAcceptor::from(config.server_tls.clone());
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (acceptor, calls) = (acceptor.clone(), calls.clone());
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(move |request| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    let reply = handler(
                        request.map(|incoming: hyper::body::Incoming| {
                            incoming
                                .map_err(|error| -> Error { error.into() })
                                .boxed_unsync()
                        }),
                        Context { request_id: 1 },
                    );
                    async move {
                        reply
                            .await
                            .map_err(|_| std::io::Error::other("test upstream failure"))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (task, count)
}
fn legacy_echo(_: Request<Body>, _: Context) -> Reply {
    Box::pin(async { Ok(response(200, "legacy")) })
}

#[tokio::test]
async fn independent_selection_dispatches_all_methods_and_raw_paths() {
    let mut config = fixture();
    let (task, _) = upstream(&mut config, legacy_echo).await;
    let legacy = Legacy::new(config);
    for enabled in [vec![], vec!["hello"], vec!["info"], vec!["hello", "info"]] {
        let dispatcher = Dispatcher::new(
            registry(),
            &enabled.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        for method in [
            "GET", "POST", "PUT", "DELETE", "OPTIONS", "PATCH", "HEAD", "CUSTOM",
        ] {
            for path in [
                "/hello",
                "/hello/",
                "/info",
                "/info/",
                "/info/child",
                "/information",
                "/info%2fchild",
                "/info%5cchild",
                "/other",
                "/hello?raw=%2f",
            ] {
                let request = Request::builder()
                    .method(method)
                    .uri(path)
                    .header("Host", "original")
                    .body(body(""))
                    .unwrap();
                let (reply, owner, error) =
                    dispatch(request, Context { request_id: 1 }, &dispatcher, &legacy).await;
                let raw_path = path.split('?').next().unwrap();
                let expected = if raw_path == "/hello" && enabled.contains(&"hello") {
                    "hello"
                } else if ["/info", "/info/", "/info/child"].contains(&raw_path)
                    && enabled.contains(&"info")
                {
                    "info"
                } else {
                    "legacy"
                };
                assert_eq!(owner, expected, "{method} {path} {enabled:?}");
                assert_eq!(error, None);
                assert_eq!(
                    reply.status(),
                    if owner == "hello" && method != "GET" {
                        405
                    } else {
                        200
                    }
                );
                let _ = reply.into_body().collect().await;
            }
        }
    }
    task.abort();
}

#[test]
fn validation_and_header_policy() {
    for value in [
        "http://localhost",
        "https://u:p@localhost",
        "https://localhost/path",
        "https://localhost/?q",
        "https://localhost/#fragment",
    ] {
        assert!(origin(value).is_err());
    }
    assert!(origin("https://localhost:8002/").is_ok());
    assert!(Dispatcher::new(registry(), &["missing".into()]).is_err());
    assert!(Dispatcher::new(registry(), &["hello".into(), "hello".into()]).is_err());
    for paths in [
        vec![Path::Exact("/hello"), Path::Exact("/hello")],
        vec![Path::Family("/info"), Path::Exact("/info/child")],
        vec![],
        vec![Path::Family("/bad/")],
        vec![Path::Exact("/bad%2f")],
    ] {
        assert!(
            Dispatcher::new(
                vec![Registration {
                    id: "test",
                    paths,
                    handler: hello
                }],
                &[]
            )
            .is_err()
        );
    }
    let mut headers = HeaderMap::new();
    headers.append("connection", "keep-alive, x-secret".parse().unwrap());
    headers.append("connection", "x-another".parse().unwrap());
    for name in [
        "x-secret",
        "x-another",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "cookie",
    ] {
        headers.insert(name, "opaque".parse().unwrap());
    }
    strip_hop_headers(&mut headers);
    assert_eq!(headers.len(), 1);
    assert_eq!(headers["cookie"], "opaque");
}

fn mechanics(request: Request<Body>, _: Context) -> Reply {
    Box::pin(async move {
        assert_eq!(request.uri(), "/raw%2fpath?q=%2B&repeat=1&repeat=2");
        assert_eq!(request.method(), "POST");
        assert_eq!(request.version(), hyper::Version::HTTP_11);
        assert_eq!(request.headers()["host"], "original.example:8002");
        assert_eq!(request.headers()["cookie"], "SESSID=opaque");
        assert!(request.headers().get("transfer-encoding").is_none());
        assert!(request.headers().get("x-secret").is_none());
        assert!(request.headers().get("forwarded").is_none());
        let length: usize = request.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let bytes = request.into_body().collect().await?.to_bytes();
        assert_eq!(length, bytes.len());
        assert_eq!(bytes, "unchanged body");
        let mut reply = Response::builder()
            .status(302)
            .header("location", "relative?x")
            .header("content-encoding", "gzip")
            .header("connection", "x-secret")
            .header("x-secret", "gone")
            .body(body(vec![0x1f, 0x8b, 0, 9]))
            .unwrap();
        reply
            .headers_mut()
            .append("set-cookie", "a=1; path=/".parse().unwrap());
        reply
            .headers_mut()
            .append("set-cookie", "b=2; path=/".parse().unwrap());
        Ok(reply)
    })
}
#[tokio::test]
async fn framing_headers_redirects_and_no_decoding() {
    let mut config = fixture();
    let (task, count) = upstream(&mut config, mechanics).await;
    let legacy = Legacy::new(config);
    for known in [true, false] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/raw%2fpath?q=%2B&repeat=1&repeat=2")
            .header("host", "original.example:8002")
            .header("cookie", "SESSID=opaque")
            .header("forwarded", "for=spoof")
            .header("connection", "x-secret")
            .header("x-secret", "gone")
            .body(body("unchanged body"))
            .unwrap();
        if known {
            request
                .headers_mut()
                .insert("content-length", "14".parse().unwrap());
        }
        let reply = legacy.send(request).await.unwrap();
        assert_eq!(reply.status(), 302);
        assert_eq!(reply.headers()["location"], "relative?x");
        assert_eq!(reply.headers()["content-encoding"], "gzip");
        assert_eq!(reply.headers().get_all("set-cookie").iter().count(), 2);
        assert!(reply.headers().get("x-secret").is_none());
        assert_eq!(
            reply.into_body().collect().await.unwrap().to_bytes(),
            [0x1f, 0x8b, 0, 9][..]
        );
    }
    assert_eq!(count.load(Ordering::Relaxed), 2);
    task.abort();
}
fn disconnect(_: Request<Body>, _: Context) -> Reply {
    Box::pin(async { Err("disconnect".into()) })
}
fn slow(_: Request<Body>, _: Context) -> Reply {
    Box::pin(async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok(response(200, "late"))
    })
}
#[tokio::test]
async fn failures_are_bounded_and_never_replayed() {
    let mut config = fixture();
    let (task, count) = upstream(&mut config, disconnect).await;
    let legacy = Legacy::new(config.clone());
    assert!(matches!(
        legacy.send(Request::new(body(""))).await,
        Err(Failure::Protocol)
    ));
    assert_eq!(count.load(Ordering::Relaxed), 1);
    task.abort();
    let (task, _) = upstream(&mut config, slow).await;
    assert!(matches!(
        Legacy::new(config.clone())
            .send(Request::new(body("")))
            .await,
        Err(Failure::Timeout)
    ));
    task.abort();
    let (task, _) = upstream(&mut config, legacy_echo).await;
    config.upstream = origin(
        &config
            .upstream
            .to_string()
            .replace("localhost", "127.0.0.1"),
    )
    .unwrap();
    assert!(matches!(
        Legacy::new(config.clone())
            .send(Request::new(body("")))
            .await,
        Err(Failure::Tls)
    ));
    task.abort();
    config.upstream = origin("https://localhost:1").unwrap();
    assert!(matches!(
        Legacy::new(config.clone())
            .send(Request::new(body("")))
            .await,
        Err(Failure::Connection)
    ));
    assert!(matches!(
        Legacy::new(config)
            .send(Request::new(body(vec![0; 1024 * 1024 + 1])))
            .await,
        Err(Failure::TooLarge)
    ));
}
#[tokio::test]
async fn body_idle_failure_occurs_after_headers() {
    let (_sender, receiver) =
        tokio::sync::mpsc::channel::<Result<hyper::body::Frame<bytes::Bytes>, Error>>(1);
    // A pending body exercises the deadline without requiring application handlers.
    struct Pending(tokio::sync::mpsc::Receiver<Result<hyper::body::Frame<bytes::Bytes>, Error>>);
    impl hyper::body::Body for Pending {
        type Data = bytes::Bytes;
        type Error = Error;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<bytes::Bytes>, Error>>> {
            self.0.poll_recv(context)
        }
    }
    let mut body = TimedBody::wrap(
        Pending(receiver).boxed_unsync(),
        Duration::from_millis(20),
        None,
    );
    assert!(body.frame().await.unwrap().is_err());
}

struct Frames(tokio::sync::mpsc::Receiver<Result<hyper::body::Frame<bytes::Bytes>, Error>>);
impl hyper::body::Body for Frames {
    type Data = bytes::Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<bytes::Bytes>, Error>>> {
        self.0.poll_recv(context)
    }
}
fn stalled_response(_: Request<Body>, _: Context) -> Reply {
    Box::pin(async {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let _ = sender
                .send(Ok(hyper::body::Frame::data(bytes::Bytes::from_static(
                    b"first",
                ))))
                .await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            let _ = sender.send(Err("disconnected".into())).await;
        });
        Ok(Response::new(Frames(receiver).boxed_unsync()))
    })
}
#[tokio::test]
async fn real_response_stream_terminates_without_replacement_or_retry() {
    for idle in [Duration::from_millis(300), Duration::from_secs(2)] {
        let mut config = fixture();
        config.body_idle = idle;
        let (task, count) = upstream(&mut config, stalled_response).await;
        let reply = Legacy::new(config)
            .send(Request::new(body("")))
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        let mut incoming = reply.into_body();
        assert_eq!(
            incoming
                .frame()
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap(),
            "first"
        );
        assert!(incoming.frame().await.unwrap().is_err());
        assert_eq!(count.load(Ordering::Relaxed), 1);
        task.abort();
    }
}

fn large_response(_: Request<Body>, _: Context) -> Reply {
    Box::pin(async { Ok(Response::new(body(vec![b'x'; 64 * 1024 * 1024]))) })
}

struct Executable {
    child: std::process::Child,
    directory: std::path::PathBuf,
}
impl Drop for Executable {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

async fn executable(config: &Config, directory: std::path::PathBuf) -> (Executable, u16) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let child = Command::new(env!("CARGO_BIN_EXE_eshop-gateway"))
        .env("PUBLIC_BIND", format!("127.0.0.1:{port}"))
        .env("MANAGEMENT_BIND", "127.0.0.1:0")
        .env("PUBLIC_CERT", directory.join("cert.pem"))
        .env("PUBLIC_KEY", directory.join("key.pem"))
        .env("LEGACY_TRUST", directory.join("cert.pem"))
        .env("LEGACY_UPSTREAM", config.upstream.to_string())
        .env("ENABLED_SLICES", "")
        .env("BODY_IDLE_SECONDS", "1")
        .env("LOG_LEVEL", "off")
        .spawn()
        .unwrap();
    let mut process = Executable { child, directory };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "gateway exited before binding"
            );
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    (process, port)
}

// Observe the server socket without reading (and thereby relieving backpressure).
#[cfg(target_os = "linux")]
fn established_queue(server: u16, client: u16) -> Option<usize> {
    fs::read_to_string("/proc/net/tcp")
        .unwrap()
        .lines()
        .skip(1)
        .find_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let local = fields[1].split(':').nth(1)?;
            let remote = fields[2].split(':').nth(1)?;
            if u16::from_str_radix(local, 16).ok()? == server
                && u16::from_str_radix(remote, 16).ok()? == client
                && fields[3] == "01"
            {
                usize::from_str_radix(fields[4].split(':').next()?, 16).ok()
            } else {
                None
            }
        })
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn executable_bounds_stalled_tls_reader_without_retry() {
    use tokio::io::AsyncWriteExt;
    let (mut config, directory) = fixture_files();
    let (task, count) = upstream(&mut config, large_response).await;
    let (_process, port) = executable(&config, directory).await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(16 * 1024).unwrap();
    let stream = socket.connect(([127, 0, 0, 1], port).into()).await.unwrap();
    let client = stream.local_addr().unwrap().port();
    let mut stream = tokio_rustls::TlsConnector::from(config.client_tls.clone())
        .connect(
            tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
        .unwrap();
    stream
        .write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        established_queue(port, client).is_some_and(|queue| queue > 0),
        "must first reproduce socket backpressure"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while established_queue(port, client).is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stalled connection retained after write deadline");
    assert_eq!(count.load(Ordering::Relaxed), 1);
    task.abort();
}

#[tokio::test]
async fn executable_allows_slow_progressing_tls_reader() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut config, directory) = fixture_files();
    let (task, count) = upstream(&mut config, large_response).await;
    let (_process, port) = executable(&config, directory).await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(64 * 1024).unwrap();
    let stream = socket.connect(([127, 0, 0, 1], port).into()).await.unwrap();
    let mut stream = tokio_rustls::TlsConnector::from(config.client_tls.clone())
        .connect(
            tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
        .unwrap();
    stream
        .write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let started = std::time::Instant::now();
    let mut received = Vec::new();
    let mut buffer = [0; 64 * 1024];
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let length = stream.read(&mut buffer).await.unwrap();
            if length == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..length]);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(started.elapsed() > Duration::from_secs(1));
    let header_end = received
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    assert!(received.starts_with(b"HTTP/1.1 200"));
    assert_eq!(received.len() - header_end, 64 * 1024 * 1024);
    assert!(received[header_end..].iter().all(|byte| *byte == b'x'));
    assert_eq!(count.load(Ordering::Relaxed), 1);
    task.abort();
}
