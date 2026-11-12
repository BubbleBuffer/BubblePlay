//! A PC object's side of the directory: the reports it sends to the front object, and the claim
//! of a record the single-object relay left behind.

use bp_relay_core::directory::{PcReport, REPORT_EVERY_MS};
use worker::wasm_bindgen::JsValue;
use worker::{Error, Method, Request, RequestInit, Result};

use crate::live::{Reported, now_ms};
use crate::pc_store::REPORT_STORED_STATUS;
use crate::store;
use crate::{INTERNAL_PREFIX, RELAY_BINDING, RELAY_NAME};

impl crate::PcRelay {
    /// The report for this object's PC, if it holds a record. `online` is the host's presence now.
    pub(crate) fn current_report(&self) -> Option<PcReport> {
        let guard = self.live.borrow();
        let live = guard.as_ref()?;
        let row = live.core.listing().into_iter().next()?;
        Some(PcReport {
            id: row.id,
            public: row.public,
            first_seen: row.first_seen,
            last_seen: row.last_seen,
            admitted: row.admitted,
            online: row.online,
        })
    }

    /// Once per object start: when this object holds no PC and the upgrade names a device id, asks
    /// the front object for that PC record from the single-object relay table, adopts it and
    /// saves it. The directory report that follows makes the front object drop it from the old
    /// table, so an existing PC keeps working after the split without registering again. The
    /// record is saved here before the old copy goes.
    pub(crate) async fn claim_legacy(&self, pc: Option<&str>) {
        let name = bp_relay_core::directory::session_object_name(pc);
        let Some(id) = name.strip_prefix("pc-") else {
            return;
        };
        let wanted = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return;
            };
            let wanted = !live.legacy_checked && live.core.pcs().is_empty();
            live.legacy_checked = true;
            wanted
        };
        if !wanted {
            return;
        }
        let Some(text) = self.fetch_legacy(id).await else {
            return;
        };
        let Ok(table) = store::decode_pcs(Some(&text)) else {
            worker::console_error!("a legacy PC record could not be read");
            return;
        };
        let saved = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return;
            };
            live.core.adopt_pcs(table);
            store::encode_pcs(live.core.pcs())
        };
        let written = match saved {
            Ok(text) => self.write_pcs(text).await.is_ok(),
            Err(_) => false,
        };
        if written {
            self.sync_directory().await;
        } else {
            self.set_store_health(false);
        }
    }

    /// The old record of one PC from the front object: its table text, or None (not there, or the
    /// front object did not answer 200).
    pub(crate) async fn fetch_legacy(&self, id: &str) -> Option<String> {
        let stub = self
            .env
            .durable_object(RELAY_BINDING)
            .ok()?
            .id_from_name(RELAY_NAME)
            .ok()?
            .get_stub()
            .ok()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from_str(id)));
        let request = Request::new_with_init(
            &format!("https://relay.internal{INTERNAL_PREFIX}dir/claim"),
            &init,
        )
        .ok()?;
        let mut response = stub.fetch_with_request(request).await.ok()?;
        if response.status_code() != 200 {
            return None;
        }
        response.text().await.ok()
    }

    /// Posts a report to the front object. The status is the answer's: 204 stored, 409 the
    /// directory is full, 400 or 5xx otherwise.
    pub(crate) async fn post_report(&self, report: &PcReport) -> Result<u16> {
        let body = serde_json::to_string(report).map_err(|e| Error::RustError(e.to_string()))?;
        let stub = self
            .env
            .durable_object(RELAY_BINDING)?
            .id_from_name(RELAY_NAME)?
            .get_stub()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from_str(&body)));
        let request = Request::new_with_init(
            &format!("https://relay.internal{INTERNAL_PREFIX}dir/report"),
            &init,
        )?;
        Ok(stub.fetch_with_request(request).await?.status_code())
    }

    /// Tells the directory about this PC when its record or presence changed since the last
    /// acknowledged report, and repeats the online report every REPORT_EVERY_MS. A failed report
    /// is retried by the alarm.
    pub(crate) async fn sync_directory(&self) {
        let Some(report) = self.current_report() else {
            return;
        };
        let now = now_ms();
        let due = {
            let guard = self.live.borrow();
            let Some(live) = guard.as_ref() else {
                return;
            };
            match live.reported {
                _ if live.report_pending => true,
                None => true,
                Some(sent) => {
                    sent.online != report.online
                        || (report.online && now.saturating_sub(sent.at_ms) >= REPORT_EVERY_MS)
                }
            }
        };
        if !due {
            return;
        }
        let answer = self.post_report(&report).await;
        let acknowledged = matches!(answer, Ok(REPORT_STORED_STATUS));
        if let Some(live) = self.live.borrow_mut().as_mut() {
            if acknowledged {
                live.reported = Some(Reported {
                    online: report.online,
                    at_ms: now,
                });
                live.report_pending = false;
                live.report_failures = 0;
            } else {
                if live.report_failures == 0 {
                    // Logged on the first failure of a run only; the retries are backed off.
                    match answer {
                        Ok(status) => worker::console_error!(
                            "the directory refused a report with status {}",
                            status
                        ),
                        Err(error) => {
                            worker::console_error!("the directory could not be reached: {}", error)
                        }
                    }
                }
                live.report_pending = true;
                live.report_failures = live.report_failures.saturating_add(1);
            }
        }
    }
}
