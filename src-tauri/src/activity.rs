//! The in-memory activity log and the response-body wrapper that makes it
//! honest.
//!
//! A download is not a point event. Logging only on request arrival produces a
//! log that says "someone requested a 4 GB video" whether the transfer
//! completed or died at 12%. `CountingBody` counts the bytes that actually
//! reached the socket and finalizes the entry on completion -- or, in `Drop`,
//! marks it aborted when the phone navigated away or Wi-Fi dropped.

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::atomic::Ordering,
    task::{Context, Poll},
};

use bytes::Buf;
use http_body::{Body, Frame};

use crate::{
    models::{ActivityEntry, ServerCtx, ACTIVITY_LOG_CAP},
    utils::now_ms,
};

impl ServerCtx {
    /// Push an entry and return its id, so a streaming response can finalize it.
    pub(crate) fn log(&self, mut entry: ActivityEntry) -> u64 {
        let id = self.next_activity_id.fetch_add(1, Ordering::SeqCst) + 1;
        entry.id = id;
        if entry.at_ms == 0 {
            entry.at_ms = now_ms();
        }
        if let Ok(mut log) = self.activity.lock() {
            log.push_front(entry);
            while log.len() > ACTIVITY_LOG_CAP {
                log.pop_back();
            }
        }
        self.requests_served.fetch_add(1, Ordering::Relaxed);
        id
    }

    pub(crate) fn finalize(&self, id: u64, bytes: u64, status: &str) {
        if let Ok(mut log) = self.activity.lock() {
            if let Some(entry) = log.iter_mut().find(|e| e.id == id) {
                entry.bytes = bytes;
                entry.status = status.to_string();
            }
        }
        self.bytes_served.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Convenience for point events (auth, listing, denial) that have no body
    /// to track.
    pub(crate) fn log_event(
        &self,
        kind: &str,
        status: &str,
        client_ip: &str,
        user_agent: &str,
        share: Option<(&str, &str)>,
        path: Option<String>,
        detail: Option<String>,
    ) {
        self.log(ActivityEntry {
            at_ms: now_ms(),
            client_ip: client_ip.to_string(),
            user_agent: user_agent.to_string(),
            kind: kind.to_string(),
            share_id: share.map(|(id, _)| id.to_string()),
            share_name: share.map(|(_, name)| name.to_string()),
            path,
            status: status.to_string(),
            detail,
            ..Default::default()
        });
    }
}

pub(crate) fn read_since(
    log: &VecDeque<ActivityEntry>,
    since_id: u64,
    limit: usize,
) -> Vec<ActivityEntry> {
    log.iter()
        .take_while(|e| e.id > since_id)
        .take(limit.clamp(1, ACTIVITY_LOG_CAP))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// CountingBody
// ---------------------------------------------------------------------------

/// Wraps a response body, counting bytes delivered and finalizing an activity
/// entry when the stream ends -- or is dropped early.
pub(crate) struct CountingBody<B> {
    inner: B,
    ctx: ServerCtx,
    entry_id: u64,
    sent: u64,
    finished: bool,
}

impl<B> CountingBody<B> {
    pub(crate) fn new(inner: B, ctx: ServerCtx, entry_id: u64) -> Self {
        Self {
            inner,
            ctx,
            entry_id,
            sent: 0,
            finished: false,
        }
    }
}

// SAFETY of the manual projection: `inner` is the only field that must be
// structurally pinned, and it is never moved out of `self` -- every other field
// is a plain counter accessed through the same `&mut`. This avoids taking a
// `pin-project-lite` dependency for one field.
impl<B> Body for CountingBody<B>
where
    B: Body + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);

        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.sent += data.remaining() as u64;
                }
            }
            Poll::Ready(Some(Err(_))) => {
                if !this.finished {
                    this.finished = true;
                    this.ctx.finalize(this.entry_id, this.sent, "error");
                }
            }
            Poll::Ready(None) => {
                if !this.finished {
                    this.finished = true;
                    this.ctx.finalize(this.entry_id, this.sent, "ok");
                }
            }
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for CountingBody<B> {
    fn drop(&mut self) {
        // The phone navigated away, or Wi-Fi dropped, mid-stream.
        if !self.finished {
            self.ctx.finalize(self.entry_id, self.sent, "aborted");
        }
    }
}
