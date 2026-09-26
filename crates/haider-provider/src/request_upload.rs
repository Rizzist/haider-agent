//! Request-body upload boundary for provider transports.
//!
//! A large request (for example one carrying a screenshot) can take longer to
//! upload than the provider takes to answer. Upload is transport work, not
//! provider silence, so the logical idle clock is paused from body
//! serialization (after credential and endpoint work in the request builder,
//! which stays on the clock) until the body producer reaches EOF, and adapters
//! start their response-open clock only after [`RequestUploadBoundary::wait`] returns.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use tokio::sync::Notify;

/// Body chunk size handed to the HTTP client. Chunking (rather than one
/// buffer) is what lets EOF approximate "the client has written the body";
/// 64 KiB keeps a multi-megabyte body to a few dozen polls.
const REQUEST_UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

/// Absolute request-upload budget. Allow a conservative 64 KiB/s, with a
/// 30-second floor for connection setup and a five-minute ceiling so a peer
/// that stops reading cannot hold the paused logical-idle clock forever.
pub(crate) fn request_upload_budget(body_bytes: usize) -> std::time::Duration {
    let seconds = body_bytes.div_ceil(64 * 1024).clamp(30, 5 * 60);
    std::time::Duration::from_secs(u64::try_from(seconds).unwrap_or(5 * 60))
}

/// Shared completion flag for one request body. Creating it pauses the
/// task's [`crate::ProviderIdleDeadline`] (if any); the first
/// [`complete`](Self::complete) — body EOF, body drop, or an explicit call on
/// an error/response path — resumes it exactly once.
#[derive(Debug, Clone)]
pub(crate) struct RequestUploadBoundary {
    complete: Arc<AtomicBool>,
    notify: Arc<Notify>,
    idle_deadline: Option<crate::ProviderIdleDeadline>,
}

impl RequestUploadBoundary {
    pub(crate) fn new() -> Self {
        let idle_deadline = crate::ProviderIdleDeadline::current();
        if let Some(idle_deadline) = &idle_deadline {
            idle_deadline.pause_for_upload();
        }
        Self {
            complete: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
            idle_deadline,
        }
    }

    /// Fixed-length streaming body that marks this boundary complete at EOF
    /// or when the client drops it (cancellation, connection error).
    pub(crate) fn body(&self, bytes: Vec<u8>) -> reqwest::Body {
        reqwest::Body::wrap_stream(RequestUploadBody {
            bytes: Bytes::from(bytes),
            offset: 0,
            boundary: self.clone(),
        })
    }

    pub(crate) fn complete(&self) {
        if self.complete.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(idle_deadline) = &self.idle_deadline {
            idle_deadline.resume_after_upload();
        }
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait(&self) {
        while !self.complete.load(Ordering::Acquire) {
            let notified = self.notify.notified();
            if self.complete.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

struct RequestUploadBody {
    bytes: Bytes,
    offset: usize,
    boundary: RequestUploadBoundary,
}

impl Stream for RequestUploadBody {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.offset == self.bytes.len() {
            self.boundary.complete();
            return Poll::Ready(None);
        }
        let end = self
            .offset
            .saturating_add(REQUEST_UPLOAD_CHUNK_BYTES)
            .min(self.bytes.len());
        let chunk = self.bytes.slice(self.offset..end);
        self.offset = end;
        Poll::Ready(Some(Ok(chunk)))
    }
}

impl Drop for RequestUploadBody {
    fn drop(&mut self) {
        self.boundary.complete();
    }
}
