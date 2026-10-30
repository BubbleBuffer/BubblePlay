//! The platform-neutral contract: `AppState` out as JSON, `Action` in as JSON. The Rust serde
//! types in `bubbleplay_client::model` are the source of the schema; the Dart models mirror
//! them and `app/test/contract` round-trips the same fixtures (`app/test/fixtures/contract`).

use anyhow::{Context, Result};
use bubbleplay_client::model::{Action, Backend, Notifier};

use crate::backend::AppBackend;

/// The backend behind JSON strings. This is what the bridge (and, on Android, the Pigeon host
/// API) calls.
pub struct Core {
    backend: AppBackend,
}

impl Core {
    pub fn new(backend: AppBackend) -> Self {
        Self { backend }
    }

    pub fn snapshot_json(&self) -> String {
        serde_json::to_string(&self.backend.snapshot()).expect("AppState serializes")
    }

    pub fn act_json(&mut self, action: &str) -> Result<()> {
        let action: Action = serde_json::from_str(action).context("not an Action")?;
        self.backend.act(action);
        Ok(())
    }

    pub fn set_notifier(&mut self, notify: Notifier) {
        self.backend.set_notifier(notify);
    }

    pub fn export_key(&self) -> Result<String> {
        Ok(self.backend.export_key()?.to_string())
    }
}
