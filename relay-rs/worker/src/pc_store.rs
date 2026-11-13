//! A PC object's own record store, and its order of writes against the directory.
//!
//! A PC whose record the object already holds persists locally and tells the directory
//! afterwards, so an unhealthy front object cannot stop a known host from registering. A PC the
//! object has never stored asks the directory first: only the directory can refuse it at the cap.

use bp_relay_core::directory::PcReport;
use worker::Result;

use crate::live::{Reported, now_ms};
use crate::store;

/// The directory answer for a stored report.
pub(crate) const REPORT_STORED_STATUS: u16 = 204;

/// The directory answer for a new PC while it holds its maximum.
pub(crate) const REPORT_FULL_STATUS: u16 = 409;

/// The storage key of this object's PC record.
pub(crate) const PCS_KEY: &str = "pcs";

/// Why a persist failed.
pub(crate) enum PersistError {
    /// The directory holds its maximum of PCs and this one is new.
    Cap,
    /// The directory or the object's storage could not be written.
    Store,
}

impl crate::PcRelay {
    /// Writes the PC table to this object's storage. A known PC is written first and reported
    /// later (`report_pending` makes the next directory sync send it). A new PC is reported first,
    /// so the directory can refuse it at the cap, and written after the directory accepted it.
    pub(crate) async fn persist_current_pcs(&self) -> std::result::Result<(), PersistError> {
        let (text, known) = {
            let guard = self.live.borrow();
            match guard.as_ref() {
                Some(live) => (store::encode_pcs(live.core.pcs()), live.stored),
                None => (Err("the relay is not running".to_owned()), false),
            }
        };
        let text = text.map_err(|_| PersistError::Store)?;
        if known {
            self.write_pcs(text)
                .await
                .map_err(|_| PersistError::Store)?;
            if let Some(live) = self.live.borrow_mut().as_mut() {
                live.report_pending = true;
            }
            return Ok(());
        }
        if let Some(report) = self.current_report() {
            self.report_new_pc(&report).await?;
        }
        self.write_pcs(text).await.map_err(|_| PersistError::Store)
    }

    /// Asks the directory to take a PC this object has not stored before.
    async fn report_new_pc(&self, report: &PcReport) -> std::result::Result<(), PersistError> {
        match self.post_report(report).await {
            Ok(REPORT_STORED_STATUS) => {
                if let Some(live) = self.live.borrow_mut().as_mut() {
                    live.reported = Some(Reported {
                        online: report.online,
                        at_ms: now_ms(),
                    });
                    live.report_pending = false;
                    live.report_failures = 0;
                }
                Ok(())
            }
            Ok(REPORT_FULL_STATUS) => Err(PersistError::Cap),
            _ => Err(PersistError::Store),
        }
    }

    /// Retries the PC write while the store is unhealthy. The alarm asks for this every second.
    pub(crate) async fn retry_store(&self) {
        let healthy = self
            .live
            .borrow()
            .as_ref()
            .is_none_or(|live| live.pcs_healthy);
        if healthy {
            return;
        }
        if self.persist_current_pcs().await.is_ok() {
            self.set_store_health(true);
        }
    }

    pub(crate) fn set_store_health(&self, healthy: bool) {
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.pcs_healthy = healthy;
            live.core.set_store_healthy(healthy);
        }
    }

    /// Writes the table text to storage. The object then holds the record for good.
    pub(crate) async fn write_pcs(&self, text: String) -> Result<()> {
        self.state.storage().put(PCS_KEY, text).await?;
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.stored = true;
        }
        Ok(())
    }
}
