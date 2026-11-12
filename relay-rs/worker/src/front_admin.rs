//! The front object's admin API, behind the admin bearer.

use serde_json::{Value, json};
use worker::{Method, Request, RequestInit, Response, Result};

use crate::front::{DIR_PREFIX, not_found, server_error};
use crate::http;
use crate::live::now_ms;
use crate::{INTERNAL_PREFIX, PC_RELAY_BINDING};

impl crate::Relay {
    /// The admin API, behind the admin bearer. A wrong bearer, route or id is 404, as native.
    pub(crate) async fn admin(
        &self,
        method: &Method,
        path: &str,
        authorization: Option<&str>,
    ) -> Result<Response> {
        let allowed = {
            let guard = self.live.borrow();
            let Some(live) = guard.as_ref() else {
                return server_error();
            };
            http::bearer_matches(authorization, &live.admin_token)
        };
        if !allowed {
            return not_found();
        }
        if *method == Method::Get && path == "/__gate/manage/pcs" {
            let rows = {
                let guard = self.live.borrow();
                let Some(live) = guard.as_ref() else {
                    return server_error();
                };
                live.directory.listing(now_ms())
            };
            let rows: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "id": row.id,
                        "public": row.public,
                        "first_seen": row.first_seen,
                        "last_seen": row.last_seen,
                        "online": row.online,
                        "admitted": row.admitted,
                    })
                })
                .collect();
            return http::json(&Value::Array(rows));
        }
        if let Some(id) = path.strip_prefix("/__gate/manage/pcs/")
            && *method == Method::Delete
            && !id.is_empty()
            && !id.contains('/')
        {
            return self.delete_pc(id).await;
        }
        not_found()
    }

    /// Forgets a PC: its object drops the record and closes its host, then the directory forgets
    /// it. An id that is not a device id, or one the directory does not hold, is 404.
    pub(crate) async fn delete_pc(&self, id: &str) -> Result<Response> {
        let Ok(parsed) = bp_device_key::DeviceId::parse(id) else {
            return not_found();
        };
        let id = parsed.to_string();
        let known = {
            let guard = self.live.borrow();
            let Some(live) = guard.as_ref() else {
                return server_error();
            };
            live.directory.is_registered(&id)
        };
        if !known {
            return not_found();
        }
        // The PC's object answers 404 when it holds no record (a PC that never reported a record
        // there): the directory entry still goes.
        let object = bp_relay_core::directory::session_object_name(Some(&id));
        let stub = self
            .env
            .durable_object(PC_RELAY_BINDING)?
            .id_from_name(&object)?
            .get_stub()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post);
        let request = Request::new_with_init(
            &format!("https://relay.internal{INTERNAL_PREFIX}pc/delete"),
            &init,
        )?;
        let status = stub.fetch_with_request(request).await?.status_code();
        if status != 204 && status != 404 {
            return http::status(503);
        }
        if let Some(live) = self.live.borrow_mut().as_mut() {
            live.directory.remove(&id);
        }
        match self
            .state
            .storage()
            .delete(&format!("{DIR_PREFIX}{id}"))
            .await
        {
            Ok(_) => http::status(204),
            Err(_) => http::status(503),
        }
    }
}
