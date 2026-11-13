//! The front object's directory side: reports from PC objects and the claim of old-table records.

use bp_relay_core::directory::{CapReached, PcReport};
use bp_relay_core::relay::PcTable;
use worker::{Method, Request, Response, Result};

use crate::front::{DIR_PREFIX, LEGACY_KEY, not_found, server_error};
use crate::http;
use crate::live::now_ms;
use crate::store;

impl crate::Relay {
    /// A PC object's report. Only the objects reach this path: the Worker answers 404 to a request
    /// from outside that names it.
    pub(crate) async fn directory_report(&self, req: Request, method: &Method) -> Result<Response> {
        if *method != Method::Post {
            return not_found();
        }
        let Some(body) = http::read_body(&req).await? else {
            return http::status(413);
        };
        let Ok(report) = serde_json::from_slice::<PcReport>(&body) else {
            return http::status(400);
        };
        if !report.is_valid() {
            return http::status(400);
        }
        let stored = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return server_error();
            };
            live.directory.report(&report, now_ms())
        };
        let entry = match stored {
            Ok(entry) => entry,
            Err(CapReached) => return http::status(409),
        };
        let Ok(text) = serde_json::to_string(&entry) else {
            return http::status(503);
        };
        match self
            .state
            .storage()
            .put(&format!("{DIR_PREFIX}{}", report.id), text)
            .await
        {
            Ok(()) => {
                self.drop_legacy(&report.id).await;
                http::status(204)
            }
            Err(_) => http::status(503),
        }
    }

    /// A PC object asks for its record from the single-object relay table (a POST whose body is
    /// the canonical device id; the answer is that one record in the table format). The record
    /// stays here until the object reports it (`drop_legacy`), so a crash in between loses nothing.
    pub(crate) async fn claim(&self, req: Request, method: &Method) -> Result<Response> {
        if *method != Method::Post {
            return not_found();
        }
        let Some(body) = http::read_body(&req).await? else {
            return http::status(413);
        };
        let id = String::from_utf8_lossy(&body).into_owned();
        let text = {
            let guard = self.live.borrow();
            let Some(live) = guard.as_ref() else {
                return server_error();
            };
            live.legacy.records.get(&id).and_then(|record| {
                let mut one = PcTable::new();
                one.records.insert(id.clone(), record.clone());
                store::encode_pcs(&one).ok()
            })
        };
        match text {
            Some(text) => Response::ok(text),
            None => not_found(),
        }
    }

    /// Forgets the old-table record of a PC whose own object has saved it, and deletes the old
    /// table key when it is empty.
    pub(crate) async fn drop_legacy(&self, id: &str) {
        let text = {
            let mut guard = self.live.borrow_mut();
            let Some(live) = guard.as_mut() else {
                return;
            };
            if live.legacy.records.remove(id).is_none() {
                return;
            }
            if live.legacy.is_empty() {
                None
            } else {
                store::encode_pcs(&live.legacy).ok()
            }
        };
        let storage = self.state.storage();
        let result = match text {
            Some(text) => storage.put(LEGACY_KEY, text).await,
            None => storage.delete(LEGACY_KEY).await.map(|_| ()),
        };
        if result.is_err() {
            worker::console_error!("the old PC table could not be updated");
        }
    }
}
