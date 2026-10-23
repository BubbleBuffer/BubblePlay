//! The stored PC table and gate records. Each is one JSON text in the Durable Object's storage,
//! in the same version-1 shape the native server writes to pcs.json and gate.json. Validation
//! follows the native store (server/src/store.rs and runtime.rs).

use std::collections::HashSet;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::DeviceId;
use bp_relay_core::gate::{GateRecord, GateSnapshot};
use bp_relay_core::limits::ADMIT_VIEWERS_MAX;
use bp_relay_core::relay::{PcRecord, PcTable};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PCS_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPcs {
    version: u32,
    pcs: Vec<StoredPc>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPc {
    id: String,
    public: String,
    registered_ms: u64,
    #[serde(default)]
    admitted: Vec<String>,
    #[serde(default, rename = "remotePairing")]
    remote_pairing: bool,
}

/// The PC table from its stored text. Missing text is an empty table.
pub fn decode_pcs(text: Option<&str>) -> Result<PcTable, String> {
    let Some(text) = text else {
        return Ok(PcTable::new());
    };
    let file: StoredPcs =
        serde_json::from_str(text).map_err(|_| "the stored PC table is corrupt".to_owned())?;
    validate(&file)?;
    let mut table = PcTable::new();
    for pc in file.pcs {
        let public = decode_public_key(&pc.public)
            .ok_or_else(|| "the stored PC table has a bad public key".to_owned())?;
        table.records.insert(
            pc.id,
            PcRecord {
                public,
                first_seen: pc.registered_ms,
                last_seen: pc.registered_ms,
                admitted: pc.admitted.into_iter().collect(),
                remote_pairing: pc.remote_pairing,
            },
        );
    }
    Ok(table)
}

/// The stored text for a PC table. Refuses an invalid table, as the native store does.
pub fn encode_pcs(table: &PcTable) -> Result<String, String> {
    let file = StoredPcs {
        version: PCS_VERSION,
        pcs: table
            .records
            .iter()
            .map(|(id, record)| StoredPc {
                id: id.clone(),
                public: URL_SAFE_NO_PAD.encode(record.public),
                registered_ms: record.first_seen,
                admitted: record.admitted.iter().cloned().collect(),
                remote_pairing: record.remote_pairing,
            })
            .collect(),
    };
    validate(&file)?;
    serde_json::to_string(&file).map_err(|_| "the PC table could not be encoded".to_owned())
}

/// The gate's cookie records from their stored text. Missing text is no records.
pub fn decode_gate(text: Option<&str>) -> Result<GateSnapshot, String> {
    let Some(text) = text else {
        return Ok(GateSnapshot::default());
    };
    let value: Value =
        serde_json::from_str(text).map_err(|_| "the stored gate records are corrupt".to_owned())?;
    gate_from_value(&value)
}

/// The stored text for the gate's cookie records (the same shape as gate.json).
pub fn encode_gate(snapshot: &GateSnapshot) -> Result<String, String> {
    let cookies: Vec<Value> = snapshot
        .records
        .iter()
        .map(|record| {
            json!({
                "hash": URL_SAFE_NO_PAD.encode(record.hash),
                "expires_ms": record.expires_ms,
            })
        })
        .collect();
    serde_json::to_string(&json!({ "version": 1, "cookies": cookies }))
        .map_err(|_| "the gate records could not be encoded".to_owned())
}

/// An unpadded base64url string of exactly 32 bytes, in canonical form.
pub fn decode_public_key(text: &str) -> Option<[u8; 32]> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let public: [u8; 32] = bytes.try_into().ok()?;
    (URL_SAFE_NO_PAD.encode(public) == text).then_some(public)
}

/// The same rules as the native validate_pcs.
fn validate(file: &StoredPcs) -> Result<(), String> {
    if file.version != PCS_VERSION {
        return Err("unsupported PC table version".to_owned());
    }
    let mut seen = HashSet::new();
    for pc in &file.pcs {
        let public = decode_public_key(&pc.public).ok_or_else(|| {
            "a PC public key is not an unpadded base64url 32-byte value".to_owned()
        })?;
        let id = DeviceId::parse(&pc.id).map_err(|_| "a PC id is not a device ID".to_owned())?;
        if id.to_string() != pc.id {
            return Err("a PC id is not in canonical form".to_owned());
        }
        if DeviceId::from_public_key(&public) != id {
            return Err("a PC id does not match its public key".to_owned());
        }
        if !seen.insert(pc.id.as_str()) {
            return Err("a PC id is listed twice".to_owned());
        }
        if pc.admitted.len() > ADMIT_VIEWERS_MAX {
            return Err("a PC has too many admitted viewers".to_owned());
        }
        let mut admitted = HashSet::new();
        for viewer in &pc.admitted {
            let canonical =
                DeviceId::parse(viewer).is_ok_and(|parsed| parsed.to_string() == *viewer);
            if !canonical || !admitted.insert(viewer.as_str()) {
                return Err("an admitted viewer is not a unique canonical ID".to_owned());
            }
        }
    }
    Ok(())
}

fn gate_from_value(value: &Value) -> Result<GateSnapshot, String> {
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("the gate records have an unsupported version".to_owned());
    }
    let cookies = value
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| "the gate records have no cookie list".to_owned())?;
    let mut records = Vec::with_capacity(cookies.len());
    for cookie in cookies {
        let hash_text = cookie
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| "a gate cookie has no hash".to_owned())?;
        let bytes = URL_SAFE_NO_PAD
            .decode(hash_text)
            .map_err(|_| "a gate cookie hash is not base64url".to_owned())?;
        let hash: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "a gate cookie hash is not 32 bytes".to_owned())?;
        let expires_ms = cookie
            .get("expires_ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| "a gate cookie has no expiry".to_owned())?;
        records.push(GateRecord { hash, expires_ms });
    }
    Ok(GateSnapshot { records })
}
