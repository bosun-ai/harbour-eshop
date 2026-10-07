use crate::{
    config::{Config, ConfigError},
    dispatch, operations,
    proxy::{LegacyUpstream, ProxyError, text},
};
use hyper::{StatusCode, server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{convert::Infallible, sync::Arc, time::Instant};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
};
use tokio_rustls::TlsAcceptor;

// Hyper discards Content-Length when Transfer-Encoding is present. Inspect the
// bounded first header block before handing it to Hyper so ambiguity is rejected.
struct PrefixedIo<T> {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: T,
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for PrefixedIo<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            std::pin::Pin::new(&mut self.prefix).poll_read(context, buffer)
        } else {
            std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
        }
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for PrefixedIo<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(context, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(context)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

async fn inspect_headers<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: T,
) -> Result<PrefixedIo<T>, std::io::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 32768 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        bytes.push(stream.read_u8().await?);
    }
    let mut length_count = 0;
    let mut transfer = false;
    for line in bytes.split(|byte| *byte == b'\n').skip(1) {
        if let Some(colon) = line.iter().position(|byte| *byte == b':') {
            let name = &line[..colon];
            if name.eq_ignore_ascii_case(b"content-length") {
                length_count += 1;
            }
            if name.eq_ignore_ascii_case(b"transfer-encoding") {
                transfer = true;
            }
        }
    }
    if length_count > 1 || (length_count > 0 && transfer) {
        stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        stream.shutdown().await?;
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(PrefixedIo {
        prefix: std::io::Cursor::new(bytes),
        inner: stream,
    })
}

/// Run bounded public/private connections; stop accepting before draining on signals.
pub async fn serve(config: Arc<Config>) -> Result<(), ConfigError> {
    let public = TcpListener::bind(config.public_bind).await?;
    let ops = TcpListener::bind(config.ops_bind).await?;
    let acceptor = TlsAcceptor::from(config.server_tls.clone());
    let permits = Arc::new(Semaphore::new(256));
    let (shutdown, _) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tracing::info!(event = "started");
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = public.accept() => {
                let (tcp, peer) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let acceptor = acceptor.clone();
                let legacy = LegacyUpstream { config: config.clone() };
                let mut stopped = shutdown.subscribe();
                tasks.spawn(async move {
                    let _permit = permit;
                    let tls = tokio::select! {
                        _ = stopped.changed() => return,
                        result = tokio::time::timeout(legacy.config.connect, acceptor.accept(tcp)) => match result { Ok(Ok(tls)) => tls, _ => return },
                    };
                    let idle = legacy.config.idle;
                    let tls = tokio::select! {
                        _ = stopped.changed() => return,
                        result = tokio::time::timeout(idle, inspect_headers(tls)) => match result { Ok(Ok(stream)) => stream, _ => return },
                    };
                    let service = service_fn(move |request| {
                        let legacy = legacy.clone();
                        async move {
                            let id = operations::request_id();
                            let method = request.method().clone();
                            let started = Instant::now();
                            let (response, error) = match dispatch::dispatch(request, &legacy).await {
                                Ok(response) => (response, "none"),
                                Err(error) => {
                                    let status = if matches!(error, ProxyError::Protocol) { StatusCode::BAD_REQUEST } else { StatusCode::BAD_GATEWAY };
                                    (text(status, "Gateway could not complete the exchange; delivery outcome may be unknown.\n"), error.category())
                                }
                            };
                            tracing::info!(id, peer = %peer, method = %method, owner = "legacy", status = response.status().as_u16(), header_ms = started.elapsed().as_millis() as u64, error);
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let mut builder = http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(idle).keep_alive(false).max_buf_size(32768);
                    let connection = builder.serve_connection(TokioIo::new(tls), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stopped.changed() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                    }
                });
            },
            accepted = ops.accept() => {
                let (tcp, _) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let legacy = LegacyUpstream { config: config.clone() };
                let mut stopped = shutdown.subscribe();
                tasks.spawn(async move {
                    let _permit = permit;
                    let service = service_fn(move |request| operations::handle(request, legacy.clone()));
                    let mut builder = http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(std::time::Duration::from_secs(5)).keep_alive(false);
                    let connection = builder.serve_connection(TokioIo::new(tcp), service);
                    tokio::pin!(connection);
                    tokio::select! { _ = &mut connection => {}, _ = stopped.changed() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; } }
                });
            }
        }
    }
    drop(public);
    drop(ops);
    let _ = shutdown.send(true);
    if tokio::time::timeout(config.drain, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    tracing::info!(event = "stopped");
    Ok(())
}
