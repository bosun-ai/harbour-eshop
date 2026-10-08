use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::rt::{Read, ReadBufCursor, Write};
use tokio::{sync::watch, time::Instant};

use crate::{
    config::Limits,
    legacy::{BoxError, GatewayBody},
};

#[derive(Clone, Default)]
struct State {
    requests: VecDeque<RequestState>,
    next_id: u64,
    write_deadline: Option<Instant>,
}

#[derive(Clone)]
struct RequestState {
    id: u64,
    deadline: Instant,
    responding: bool,
    complete: bool,
}

/// Tracks requests until their final bytes have left Hyper and the TLS writer.
#[derive(Clone)]
pub(crate) struct RequestDeadlines {
    state: watch::Sender<State>,
    idle_ms: u64,
}

impl RequestDeadlines {
    pub fn new(idle_ms: u64) -> Self {
        Self {
            state: watch::channel(State::default()).0,
            idle_ms,
        }
    }

    pub fn start(&self, total_ms: u64) -> (u64, Instant) {
        let mut id = 0;
        let deadline = Instant::now() + Limits::duration(total_ms);
        self.state.send_modify(|state| {
            id = state.next_id;
            state.next_id += 1;
            state.requests.push_back(RequestState {
                id,
                deadline,
                responding: false,
                complete: false,
            });
        });
        (id, deadline)
    }

    pub fn response(&self, id: u64, body: GatewayBody, head: bool, timed_out: bool) -> GatewayBody {
        self.state.send_modify(|state| {
            if let Some(request) = state.requests.iter_mut().find(|request| request.id == id) {
                request.responding = true;
                if timed_out {
                    // Allow the locally generated 504 a bounded final flush after
                    // the handler has spent its total budget, even with write progress.
                    request.deadline = Instant::now() + Limits::duration(self.idle_ms);
                }
            }
        });
        let remaining = body.size_hint().exact();
        // Hyper does not poll HEAD bodies or bodies whose declared length is zero.
        if head || body.is_end_stream() || remaining == Some(0) {
            self.complete(id);
        }
        ResponseBody {
            body,
            deadlines: self.clone(),
            id,
            remaining,
        }
        .boxed_unsync()
    }

    fn complete(&self, id: u64) {
        self.state.send_if_modified(|state| {
            if let Some(request) = state.requests.iter_mut().find(|request| request.id == id) {
                if request.complete {
                    return false;
                }
                request.complete = true;
                return true;
            }
            false
        });
    }

    fn writing(&self, progress: bool) {
        self.state.send_if_modified(|state| {
            if state.requests.iter().any(|request| request.responding)
                && (progress || state.write_deadline.is_none())
            {
                state.write_deadline = Some(Instant::now() + Limits::duration(self.idle_ms));
                return true;
            }
            false
        });
    }

    fn flushed(&self) {
        self.state.send_if_modified(|state| {
            let mut changed = state.write_deadline.is_some();
            while state
                .requests
                .front()
                .is_some_and(|request| request.complete)
            {
                state.requests.pop_front();
                changed = true;
            }
            state.write_deadline = None;
            changed
        });
    }

    /// An independent timer remains polled even when Hyper is blocked on output.
    pub async fn expired(&self) {
        let mut receiver = self.state.subscribe();
        loop {
            let deadline = {
                let state = receiver.borrow_and_update();
                state
                    .requests
                    .iter()
                    // The handler owns pre-header expiry and generates the 504.
                    .filter(|request| request.responding)
                    .map(|request| request.deadline)
                    .chain(state.write_deadline)
                    .min()
            };
            match deadline {
                Some(deadline) => tokio::select! {
                    biased;
                    _ = receiver.changed() => {},
                    _ = tokio::time::sleep_until(deadline) => return,
                },
                None => {
                    let _ = receiver.changed().await;
                }
            }
        }
    }
}

struct ResponseBody {
    body: GatewayBody,
    deadlines: RequestDeadlines,
    id: u64,
    remaining: Option<u64>,
}

impl Body for ResponseBody {
    type Data = bytes::Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(context);
        if let Poll::Ready(Some(Ok(frame))) = &result
            && let Some(data) = frame.data_ref()
            && let Some(remaining) = &mut self.remaining
        {
            *remaining = remaining.saturating_sub(data.len() as u64);
        }
        if self.body.is_end_stream()
            || self.remaining == Some(0)
            || matches!(result, Poll::Ready(None))
        {
            self.deadlines.complete(self.id);
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// Observes successful socket/TLS flushes, not merely response-body exhaustion.
pub(crate) struct DeadlineIo<Inner> {
    pub inner: Inner,
    pub deadlines: RequestDeadlines,
}

impl<Inner: Read + Unpin> Read for DeadlineIo<Inner> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl<Inner: Write + Unpin> Write for DeadlineIo<Inner> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.deadlines.writing(false);
        let result = Pin::new(&mut self.inner).poll_write(context, buffer);
        if matches!(result, Poll::Ready(Ok(count)) if count > 0) {
            self.deadlines.writing(true);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(context);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.deadlines.flushed();
        } else if result.is_pending() {
            self.deadlines.writing(false);
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy::full;
    use std::future::poll_fn;

    struct Writer {
        block_write: bool,
        block_flush: bool,
    }

    impl Write for Writer {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.block_write {
                Poll::Pending
            } else {
                Poll::Ready(Ok(buffer.len()))
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.block_flush {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct ExactBody(Option<bytes::Bytes>);
    impl Body for ExactBody {
        type Data = bytes::Bytes;
        type Error = BoxError;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            match self.0.take() {
                Some(bytes) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
                None => Poll::Pending,
            }
        }
        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(4)
        }
    }

    #[tokio::test]
    async fn exact_length_completion_does_not_require_an_eof_poll() {
        let deadlines = RequestDeadlines::new(200);
        let (id, _) = deadlines.start(20);
        let mut body = deadlines.response(
            id,
            ExactBody(Some(bytes::Bytes::from_static(b"last"))).boxed_unsync(),
            false,
            false,
        );
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "last"
        );
        assert!(deadlines.state.borrow().requests.front().unwrap().complete);
        deadlines.flushed();
        assert!(
            tokio::time::timeout(Limits::duration(40), deadlines.expired())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn blocked_output_expires_without_body_polling() {
        for block_write in [true, false] {
            let deadlines = RequestDeadlines::new(20);
            let (id, _) = deadlines.start(200);
            let mut writer = DeadlineIo {
                inner: Writer {
                    block_write,
                    block_flush: true,
                },
                deadlines: deadlines.clone(),
            };
            let body = deadlines.response(id, full("final frame"), false, false);
            if !block_write {
                // Hyper has consumed the final frame, but TLS still has output to flush.
                assert_eq!(body.collect().await.unwrap().to_bytes(), "final frame");
                assert!(deadlines.state.borrow().requests.front().unwrap().complete);
            }
            poll_fn(|context| {
                if block_write {
                    assert!(
                        Pin::new(&mut writer)
                            .poll_write(context, b"output")
                            .is_pending()
                    );
                } else {
                    assert!(Pin::new(&mut writer).poll_flush(context).is_pending());
                }
                Poll::Ready(())
            })
            .await;
            tokio::time::timeout(Limits::duration(100), deadlines.expired())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn total_deadline_and_idle_keepalive() {
        let deadlines = RequestDeadlines::new(1000);
        let (id, _) = deadlines.start(20);
        let body = deadlines.response(id, full(""), false, false);
        tokio::time::timeout(Limits::duration(100), deadlines.expired())
            .await
            .unwrap();
        body.collect().await.unwrap();
        deadlines.flushed();
        assert!(
            tokio::time::timeout(Limits::duration(40), deadlines.expired())
                .await
                .is_err()
        );
        let (id, _) = deadlines.start(200);
        deadlines
            .response(id, full("next response"), false, false)
            .collect()
            .await
            .unwrap();
        deadlines.flushed();
        assert!(deadlines.state.borrow().requests.is_empty());
        let (id, _) = deadlines.start(20);
        let _head_body = deadlines.response(id, full("not polled by Hyper"), true, false);
        deadlines.flushed();
        assert!(deadlines.state.borrow().requests.is_empty());
    }

    #[tokio::test]
    async fn pre_header_timeout_gets_bounded_error_flush() {
        let deadlines = RequestDeadlines::new(40);
        let (id, deadline) = deadlines.start(5);
        tokio::time::sleep_until(deadline).await;
        assert!(
            tokio::time::timeout(Limits::duration(20), deadlines.expired())
                .await
                .is_err()
        );
        let body = deadlines.response(id, full("upstream_deadline"), false, true);
        assert!(
            tokio::time::timeout(Limits::duration(10), deadlines.expired())
                .await
                .is_err()
        );
        assert_eq!(
            body.collect().await.unwrap().to_bytes(),
            "upstream_deadline"
        );
        // Final output remains bounded even if the writer keeps making progress.
        let flush_deadline = deadlines.state.borrow().requests.front().unwrap().deadline;
        deadlines.writing(true);
        assert_eq!(
            deadlines.state.borrow().requests.front().unwrap().deadline,
            flush_deadline
        );
        tokio::time::timeout(Limits::duration(100), deadlines.expired())
            .await
            .unwrap();
        deadlines.flushed();
        assert!(deadlines.state.borrow().requests.is_empty());
    }
}
