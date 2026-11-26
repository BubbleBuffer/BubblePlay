//! Small per-session helpers of the serve loop: local clipboard polling and the session log
//! markers. Moved out of `main.rs` unchanged.
use super::{info, Duration, InputSink, Instant, Signaling};

/// Host-to-browser clipboard sync for the unprivileged local host. Text
/// copied while no viewer is live is never sent: each gap re-baselines.
#[derive(Default)]
pub(super) struct LocalClipboardPoll {
    next_poll: Option<Instant>,
    live: bool,
    id: u32,
}

impl LocalClipboardPoll {
    const INTERVAL: Duration = Duration::from_millis(300);

    pub(super) fn poll(
        &mut self,
        viewer_live: bool,
        now: Instant,
        input: &mut InputSink,
        signaling: &mut Signaling,
    ) {
        if !viewer_live {
            self.live = false;
            return;
        }
        if self.live && self.next_poll.is_some_and(|at| now < at) {
            return;
        }
        let reset = !std::mem::replace(&mut self.live, true);
        self.next_poll = Some(now + Self::INTERVAL);
        let Some(text) = input.poll_clipboard(reset) else {
            return;
        };
        self.id = self.id.wrapping_add(1);
        let (png, text) = text.wire();
        let chunks = bubbleplay_host::clipboard::chunk_text(
            &text,
            bubbleplay_host::clipboard::HOST_CHUNK_BYTES,
        );
        let count = chunks.len() as u16;
        for (index, chunk) in chunks.into_iter().enumerate() {
            signaling.send_clipboard_content_chunk(self.id, index as u16, count, chunk, png);
        }
    }
}

/// Clears the log's active session id when the serve loop ends, however it ends.
pub(super) struct SessionLogScope;

impl Drop for SessionLogScope {
    fn drop(&mut self) {
        bubbleplay_host::logging::set_session(None);
    }
}

/// One info line per viewer session that used monitor previews.
pub(super) fn log_preview_summary(stats: bubbleplay_host::monitor_preview_capture::ServiceStats) {
    info!(
        sent = stats.sender.sent,
        bytes = stats.sender.bytes,
        skipped_unchanged = stats.sender.skipped_unchanged,
        skipped_gated = stats.sender.skipped_gated,
        skipped_budget = stats.sender.skipped_budget,
        capture_failures = stats.capture_failures,
        max_capture_ms = stats.max_capture_ms,
        "monitor previews: session summary"
    );
}

/// Logs the tray's viewer markers: "viewer session started" once a viewer's
/// media transport (RTC or WSS) is first up in this session, and "viewer
/// session ended" when the session ends by any path (peer left, error). The
/// tray uses these edges for lifecycle logging; current admin demand is
/// published independently so filtering INFO cannot suppress startup.
#[derive(Default)]
pub(super) struct ViewerSessionLog {
    live: bool,
}

impl ViewerSessionLog {
    pub(super) fn observe(&mut self, connected: bool) {
        if connected && !self.live {
            self.live = true;
            info!(
                "{}",
                bubbleplay_host::admin_agent::client::VIEWER_STARTED_MARKER
            );
        } else if !connected && self.live {
            self.live = false;
            info!(
                "{}",
                bubbleplay_host::admin_agent::client::VIEWER_ENDED_MARKER
            );
        }
    }
}

impl Drop for ViewerSessionLog {
    fn drop(&mut self) {
        if self.live {
            info!(
                "{}",
                bubbleplay_host::admin_agent::client::VIEWER_ENDED_MARKER
            );
        }
    }
}
