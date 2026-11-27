//! Placing a finished upload batch: every announced file saved and verified, or nothing (see
//! `docs/architecture/file-transfer.md`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::{lock, Batch, Session, Shared, Socket};
use crate::file_transfer::Purpose;

impl Session {
    /// Every announced file of the batch is saved and verified.
    pub(super) fn batch_complete(&self) -> bool {
        !self.batch.saved.is_empty() && self.batch.saved.len() == self.batch.expected
    }

    /// A file of the batch failed or went missing: nothing of it reaches the clipboard, a paste
    /// or a drop, and the host refuses its later offers.
    pub(super) fn abandon_batch(&mut self, shared: &Mutex<Shared>, batch_id: &str) {
        if self.batch.id.as_deref() == Some(batch_id) {
            self.batch = Batch::default();
        }
        lock(shared).auth.forget_batch(batch_id);
    }

    /// Stops what the viewer cancelled over the sealed channel and what outlived its deadline.
    pub(super) fn sweep_dead_batches(
        &mut self,
        ws: &mut Socket,
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        let now = Instant::now();
        let alive = |id: &str| lock(shared).auth.has_batch(id, now);
        if let Some(receiving) = &self.receiving {
            let id = receiving.state.transfer_id;
            if !alive(&receiving.meta.batch_id) {
                info!(
                    transfer = id,
                    "upload stopped: its batch was cancelled or expired"
                );
                self.fail_upload(Some(ws), id, "cancelled", shared);
            }
        }
        if self
            .drop_upload
            .as_ref()
            .is_some_and(|drop| !alive(&drop.id))
        {
            self.cancel_drop(ws, "cancelled", shared)?;
        }
        if self.batch.id.as_deref().is_some_and(|id| !alive(id)) {
            self.batch = Batch::default();
        }
        Ok(())
    }

    /// The batch is complete: put the saved files on the clipboard, and ask for a
    /// paste when the batch was a paste.
    pub(super) fn finish_batch(&mut self, shared: &Mutex<Shared>) {
        self.finish_batch_with(shared, self.set_files);
    }

    pub(super) fn finish_batch_with(
        &mut self,
        shared: &Mutex<Shared>,
        set_files: impl FnOnce(&[PathBuf]) -> Result<u32>,
    ) {
        let batch = std::mem::take(&mut self.batch);
        if batch.saved.is_empty() {
            return;
        }
        if !self.config.clipboard {
            info!(
                files = batch.saved.len(),
                "clipboard placement skipped (BUBBLEPLAY_FILE_CLIPBOARD=0)"
            );
            return;
        }
        // The upload belongs to the viewer this socket is bound to. A newer
        // viewer must never see its files on the clipboard or a paste for them.
        if !lock(shared).is_bound(&self.bound) {
            info!(
                files = batch.saved.len(),
                "uploaded files not placed: viewer changed"
            );
            return;
        }
        // EmptyClipboard may message a hung owner. Never hold the mutex the
        // input/session thread uses for take_outbox, epochs and rate updates.
        match set_files(&batch.saved) {
            Ok(seq) => {
                let mut state = lock(shared);
                // Recorded even when stale, so our own write is never offered back.
                state.own_clipboard_seq = Some(seq);
                if !state.is_bound(&self.bound) {
                    info!(
                        files = batch.saved.len(),
                        "uploaded files were placed while the viewer changed; no paste"
                    );
                    return;
                }
                info!(
                    files = batch.saved.len(),
                    "uploaded files placed on the clipboard"
                );
                if batch.purpose == Some(Purpose::Paste) {
                    state.outbox.pastes += 1;
                }
            }
            Err(error) => {
                warn!(error = %format!("{error:#}"), "cannot place uploaded files on the clipboard")
            }
        }
    }
}
