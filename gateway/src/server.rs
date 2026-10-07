//! Bounded public TLS listener and stream lifecycle; no application behavior.
use crate::{
    Body,
    config::{Config, duration},
    dispatch::{DispatchRegistry, RequestContext, TransportFailure},
    legacy::{DriverGuard, LegacyUpstream},
    operations,
};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Instant, Sleep, sleep_until, timeout, timeout_at},
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

/// Streaming byte-count/idle/deadline guard. Failures after headers close the stream.
pub struct GuardedBody {
    inner: Body,
    context: RequestContext,
    timer: Pin<Box<Sleep>>,
    idle_ms: u64,
    remaining: Option<u64>,
    done: bool,
    _driver: Option<DriverGuard>,
}
impl GuardedBody {
    /// Preserve declared framing while bounding time and cancelling driver on drop.
    pub fn new(
        inner: Body,
        context: RequestContext,
        idle_ms: u64,
        remaining: Option<u64>,
        driver: Option<DriverGuard>,
    ) -> Self {
        let wake = context.deadline.min(Instant::now() + duration(idle_ms));
        Self {
            inner,
            context,
            timer: Box::pin(sleep_until(wake)),
            idle_ms,
            remaining,
            done: false,
            _driver: driver,
        }
    }
}
impl HttpBody for GuardedBody {
    type Data = Bytes;
    type Error = TransportFailure;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, TransportFailure>>> {
        if self.done {
            return Poll::Ready(None);
        }
        let failure = if self.context.cancellation.is_cancelled() {
            Some(TransportFailure::Cancelled)
        } else if self.timer.as_mut().poll(context).is_ready() {
            Some(TransportFailure::Deadline)
        } else {
            None
        };
        if let Some(error) = failure {
            self.done = true;
            crate::operations::stream_failure(&self.context.correlation_id, error);
            return Poll::Ready(Some(Err(error)));
        }
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if let Some(remaining) = &mut self.remaining {
                        if data.len() as u64 > *remaining {
                            self.done = true;
                            crate::operations::stream_failure(
                                &self.context.correlation_id,
                                TransportFailure::Framing,
                            );
                            return Poll::Ready(Some(Err(TransportFailure::Framing)));
                        }
                        *remaining -= data.len() as u64;
                    }
                } else {
                    self.done = true;
                    crate::operations::stream_failure(
                        &self.context.correlation_id,
                        TransportFailure::Framing,
                    );
                    return Poll::Ready(Some(Err(TransportFailure::Framing)));
                }
                let wake = self
                    .context
                    .deadline
                    .min(Instant::now() + duration(self.idle_ms));
                self.timer.as_mut().reset(wake);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                self.done = true;
                if self.remaining.is_some_and(|remaining| remaining != 0) {
                    crate::operations::stream_failure(
                        &self.context.correlation_id,
                        TransportFailure::Framing,
                    );
                    Poll::Ready(Some(Err(TransportFailure::Framing)))
                } else {
                    Poll::Ready(None)
                }
            }
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                crate::operations::stream_failure(&self.context.correlation_id, error);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
    fn size_hint(&self) -> SizeHint {
        match self.remaining {
            Some(length) => SizeHint::with_exact(length),
            None => self.inner.size_hint(),
        }
    }
    fn is_end_stream(&self) -> bool {
        self.done || (self.remaining == Some(0) && self.inner.is_end_stream())
    }
}

static IDS: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ResponseTiming {
    deadline: Instant,
    idle_until: Instant,
    idle_ms: u64,
    body_done: bool,
    correlation_id: String,
}

// Keep timing alive until Hyper has flushed the final bytes, not just read them.
struct ResponseBody {
    inner: Body,
    timing: watch::Sender<Option<ResponseTiming>>,
}
impl HttpBody for ResponseBody {
    type Data = Bytes;
    type Error = TransportFailure;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, TransportFailure>>> {
        let result = Pin::new(&mut self.inner).poll_frame(context);
        if matches!(result, Poll::Ready(None))
            || self.inner.is_end_stream()
            || self.inner.size_hint().exact() == Some(0)
        {
            self.timing.send_modify(|timing| {
                if let Some(timing) = timing {
                    timing.body_done = true;
                }
            });
        }
        result
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

struct ResponseIo<Socket> {
    inner: Socket,
    timing: watch::Sender<Option<ResponseTiming>>,
}
impl<Socket: AsyncRead + Unpin> AsyncRead for ResponseIo<Socket> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}
impl<Socket: AsyncWrite + Unpin> AsyncWrite for ResponseIo<Socket> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(context, bytes);
        if matches!(result, Poll::Ready(Ok(written)) if written > 0) {
            self.timing.send_modify(|timing| {
                if let Some(timing) = timing {
                    timing.idle_until = Instant::now() + duration(timing.idle_ms);
                }
            });
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(context);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.timing.send_if_modified(|timing| {
                if timing.as_ref().is_some_and(|timing| timing.body_done) {
                    *timing = None;
                    true
                } else {
                    false
                }
            });
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

async fn response_expired(mut timing: watch::Receiver<Option<ResponseTiming>>) {
    loop {
        let current = timing.borrow_and_update().clone();
        if let Some(current) = current {
            tokio::select! {
                changed = timing.changed() => { if changed.is_err() { return; } }
                _ = sleep_until(current.deadline.min(current.idle_until)) => {
                    operations::stream_failure(&current.correlation_id, TransportFailure::Deadline);
                    return;
                }
            }
        } else if timing.changed().await.is_err() {
            return;
        }
    }
}

/// Validate framing before connecting to Harbour; reject chunked/Expect/upgrade.
pub fn request_length(request: &Request<Incoming>, config: &Config) -> Result<u64, u16> {
    if request.headers().contains_key("transfer-encoding")
        || request.headers().contains_key("expect")
        || request.headers().contains_key("upgrade")
        || request.uri().scheme().is_some()
        || request.uri().authority().is_some()
    {
        return Err(400);
    }
    let values: Vec<_> = request.headers().get_all("content-length").iter().collect();
    if values.len() > 1 {
        return Err(400);
    }
    let length = match values.first() {
        Some(value) => {
            let text = value.to_str().map_err(|_| 400u16)?;
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(400);
            }
            text.parse::<u64>().map_err(|_| 400u16)?
        }
        None => 0,
    };
    if length > config.max_body_bytes {
        return Err(413);
    }
    let hosts: Vec<_> = request.headers().get_all("host").iter().collect();
    if hosts.len() != 1
        || hosts[0]
            .to_str()
            .ok()
            .and_then(|host| host.parse::<hyper::http::uri::Authority>().ok())
            .is_none_or(|host| host.as_str().contains('@'))
    {
        return Err(400);
    }
    // Connection must not be allowed to erase end-to-end framing or authority.
    if request.headers().get_all("connection").iter().any(|value| {
        value.to_str().unwrap_or("").split(',').any(|name| {
            ["host", "content-length"].contains(&name.trim().to_ascii_lowercase().as_str())
        })
    }) {
        return Err(400);
    }
    Ok(length)
}

async fn serve_request(
    mut request: Request<Incoming>,
    peer: std::net::SocketAddr,
    config: Arc<Config>,
    registry: Arc<DispatchRegistry>,
    cancellation: CancellationToken,
    timing: watch::Sender<Option<ResponseTiming>>,
) -> Result<Response<Body>, Infallible> {
    let started = Instant::now();
    let id = format!(
        "{:x}-{:x}",
        std::process::id(),
        IDS.fetch_add(1, Ordering::Relaxed)
    );
    let length = match request_length(&request, &config) {
        Ok(length) => length,
        Err(status) => {
            operations::log(
                &id,
                request.method().as_str(),
                "gateway",
                status,
                started,
                Some("request_policy"),
            );
            let mut response = crate::response(status, "request rejected\n");
            response.headers_mut().insert(
                "connection",
                hyper::header::HeaderValue::from_static("close"),
            );
            return Ok(response);
        }
    };
    let mut client_ip = peer.ip();
    if config.trusted_proxies.contains(&peer.ip()) {
        // Exact-IP trust policy: a single canonical address, no ambiguous chains.
        if let Some(ip) = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
        {
            client_ip = ip;
        }
    }
    let names: Vec<_> = request
        .headers()
        .keys()
        .filter(|name| {
            name.as_str() == "forwarded"
                || name.as_str().starts_with("x-forwarded-")
                || name.as_str() == "x-request-id"
        })
        .cloned()
        .collect();
    for name in names {
        request.headers_mut().remove(name);
    }
    // Strip client hop nominations before inserting gateway-owned metadata.
    crate::legacy::strip_hop_headers(request.headers_mut());
    request.headers_mut().insert(
        "x-forwarded-for",
        client_ip.to_string().parse().expect("IP header"),
    );
    request.headers_mut().insert(
        "x-forwarded-proto",
        hyper::header::HeaderValue::from_static("https"),
    );
    request
        .headers_mut()
        .insert("x-request-id", id.parse().expect("generated ID"));
    let context = RequestContext {
        correlation_id: id.clone(),
        client_ip,
        deadline: started + duration(config.deadline_ms),
        cancellation,
    };
    let (parts, body) = request.into_parts();
    let body = GuardedBody::new(
        body.map_err(|_| TransportFailure::Framing).boxed_unsync(),
        context.clone(),
        config.body_idle_ms,
        Some(length),
        None,
    )
    .boxed_unsync();
    let mut request = Request::from_parts(parts, body);
    request.headers_mut().insert(
        "content-length",
        length.to_string().parse().expect("length header"),
    );
    let method = request.method().as_str().to_owned();
    let (owner, handler) = registry.select(request.uri().path(), request.method());
    let deadline = context.deadline;
    let result = timeout_at(deadline, handler.handle(request, context)).await;
    let (response, error) = match result {
        Ok(Ok(response)) => (response, None),
        Ok(Err(TransportFailure::Deadline)) | Err(_) => (
            crate::response(504, "upstream deadline\n"),
            Some("deadline"),
        ),
        Ok(Err(_)) => (
            crate::response(502, "upstream transport failure\n"),
            Some("upstream"),
        ),
    };
    operations::log(
        &id,
        &method,
        owner,
        response.status().as_u16(),
        started,
        error,
    );
    let (parts, body) = response.into_parts();
    timing.send_replace(Some(ResponseTiming {
        deadline: if error.is_none() {
            deadline
        } else {
            Instant::now() + duration(config.body_idle_ms)
        },
        idle_until: Instant::now() + duration(config.body_idle_ms),
        idle_ms: config.body_idle_ms,
        body_done: body.is_end_stream()
            || body.size_hint().exact() == Some(0)
            || method == "HEAD"
            || parts.status == hyper::StatusCode::NO_CONTENT
            || parts.status == hyper::StatusCode::NOT_MODIFIED,
        correlation_id: id,
    }));
    Ok(Response::from_parts(
        parts,
        ResponseBody {
            inner: body,
            timing,
        }
        .boxed_unsync(),
    ))
}

/// Bind both listeners, bound connections (including TLS), and drain on SIGTERM.
pub async fn run(
    config: Config,
    upstream: Arc<LegacyUpstream>,
    registry: Arc<DispatchRegistry>,
) -> Result<(), &'static str> {
    let tls = TlsAcceptor::from(config.server_tls()?);
    let public = TcpListener::bind(config.public_bind)
        .await
        .map_err(|_| "public_bind")?;
    let management = TcpListener::bind(config.management_bind)
        .await
        .map_err(|_| "management_bind")?;
    let config = Arc::new(config);
    let slots = Arc::new(Semaphore::new(config.max_connections));
    let management_slots = Arc::new(Semaphore::new(8));
    let shutdown = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "signal")?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = public.accept() => {
                let (socket, peer) = accepted.map_err(|_| "public_accept")?;
                let Ok(permit) = slots.clone().try_acquire_owned() else { drop(socket); continue; };
                let tls = tls.clone(); let config = config.clone(); let registry = registry.clone(); let shutdown = shutdown.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let Ok(Ok(socket)) = timeout(duration(config.header_ms), tls.accept(socket)).await else { return; };
                    let cancellation = CancellationToken::new();
                    let token = cancellation.clone();
                    let header_ms = config.header_ms;
                    let max_headers = config.max_headers;
                    let max_header_bytes = config.max_header_bytes;
                    let (timing, expiry) = watch::channel(None);
                    let socket = ResponseIo { inner: socket, timing: timing.clone() };
                    let service = service_fn(move |request| serve_request(request, peer, config.clone(), registry.clone(), token.clone(), timing.clone()));
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(duration(header_ms))
                        .max_headers(max_headers).max_buf_size(max_header_bytes);
                    let connection = builder.serve_connection(TokioIo::new(socket), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = response_expired(expiry) => {},
                        _ = async {
                            tokio::select! { _ = &mut connection => {}, _ = shutdown.cancelled() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; } }
                        } => {},
                    }
                    cancellation.cancel();
                    // Connection drop also releases the response's upstream driver.
                });
            },
            accepted = management.accept() => {
                let (socket, _) = accepted.map_err(|_| "management_accept")?;
                let Ok(permit) = management_slots.clone().try_acquire_owned() else { continue; };
                let upstream = upstream.clone(); let config = config.clone(); let shutdown = shutdown.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let service = service_fn(move |request| operations::management(request, upstream.clone(), config.clone()));
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(TokioTimer::new()).header_read_timeout(duration(3000)).max_headers(16).max_buf_size(8192);
                    let connection = builder.serve_connection(TokioIo::new(socket), service);
                    tokio::pin!(connection);
                    tokio::select! { _ = &mut connection => {}, _ = shutdown.cancelled() => { connection.as_mut().graceful_shutdown(); let _ = connection.await; } }
                });
            }
        }
    }
    drop(public);
    drop(management);
    shutdown.cancel();
    if timeout(duration(config.drain_ms), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    Ok(())
}
