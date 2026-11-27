//! The sealed control messages of the file channel (viewer -> host `file-batch` and
//! `file-request`, host -> viewer `file-batch-ack`) and the id helpers they share with the
//! file packets. The host enforces them in `file_auth`; see `docs/architecture/file-transfer.md`.

use serde::{Deserialize, Serialize};

use crate::file_transfer::{
    DropPoint, Purpose, MAX_BATCH_FILES, MAX_FILE_BYTES, MAX_MIME_BYTES, MAX_NAME_BYTES,
    PROTOCOL_VERSION, SHA256_BYTES,
};

/// A batch or offer id: 32 lowercase hex digits.
pub fn valid_id128(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A fixed id for tests.
#[cfg(test)]
pub fn test_id(n: u32) -> String {
    format!("{n:032x}")
}

/// A fresh random batch/offer id (128 bits, from the OS generator).
pub fn random_id128() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    to_hex(&bytes)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_sha256_hex(text: &str) -> Option<[u8; SHA256_BYTES]> {
    if text.len() != SHA256_BYTES * 2
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut out = [0u8; SHA256_BYTES];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// --- sealed control messages (client -> host) ----------------------------------------------------

/// Largest `file-batch` text the host parses (64 files with names of up to 1024 bytes).
pub const MAX_BATCH_TEXT_BYTES: usize = 192 * 1024;

/// One announced file: the host accepts exactly these bytes for it, nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnouncedFile {
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub mtime_ms: u64,
    pub sha256: [u8; SHA256_BYTES],
}

/// Client -> host `file-batch`, sent over the sealed control channel before the batch's
/// OFFER packets use the file socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchAnnouncement {
    pub batch_id: String,
    pub purpose: Purpose,
    pub drop: Option<DropPoint>,
    pub files: Vec<AnnouncedFile>,
}

/// Client -> host `file-request`: the viewer asks for these items of a host offer; the
/// host answers only REQUEST packets that this authorizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    pub offer_id: String,
    pub ids: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientControl {
    Batch(BatchAnnouncement),
    Request(DownloadRequest),
    /// `file-batch-cancel`: the viewer gave up on this batch.
    Cancel(String),
}

/// Parses a decrypted control text; `None` when it is not a file control or is malformed.
pub fn parse_client_control(text: &str) -> Option<ClientControl> {
    if !text.contains("\"file-") || text.len() > MAX_BATCH_TEXT_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    if object.get("protocol")?.as_u64()? != u64::from(PROTOCOL_VERSION) {
        return None;
    }
    match object.get("type")?.as_str()? {
        "file-batch" => {
            let only = ["type", "protocol", "batchId", "purpose", "drop", "files"];
            if object.keys().any(|key| !only.contains(&key.as_str())) {
                return None;
            }
            let batch_id = object.get("batchId")?.as_str()?.to_owned();
            let purpose: Purpose = serde_json::from_value(object.get("purpose")?.clone()).ok()?;
            let drop: Option<DropPoint> = match object.get("drop") {
                Some(point) => Some(serde_json::from_value(point.clone()).ok()?),
                None => None,
            };
            let list = object.get("files")?.as_array()?;
            if !valid_id128(&batch_id)
                || list.is_empty()
                || list.len() > MAX_BATCH_FILES as usize
                || (purpose == Purpose::Drop) != drop.is_some()
                || drop.is_some_and(|point| point.generation == 0)
            {
                return None;
            }
            let files = list
                .iter()
                .map(announced_file)
                .collect::<Option<Vec<_>>>()?;
            Some(ClientControl::Batch(BatchAnnouncement {
                batch_id,
                purpose,
                drop,
                files,
            }))
        }
        "file-batch-cancel" => {
            if object
                .keys()
                .any(|key| !["type", "protocol", "batchId"].contains(&key.as_str()))
            {
                return None;
            }
            let batch_id = object.get("batchId")?.as_str()?;
            valid_id128(batch_id).then(|| ClientControl::Cancel(batch_id.to_owned()))
        }
        "file-request" => {
            let only = ["type", "protocol", "offerId", "ids"];
            if object.keys().any(|key| !only.contains(&key.as_str())) {
                return None;
            }
            let offer_id = object.get("offerId")?.as_str()?.to_owned();
            let list = object.get("ids")?.as_array()?;
            if !valid_id128(&offer_id) || list.is_empty() || list.len() > MAX_BATCH_FILES as usize {
                return None;
            }
            let ids = list
                .iter()
                .map(|id| id.as_u64().and_then(|id| u32::try_from(id).ok()))
                .collect::<Option<Vec<u32>>>()?;
            Some(ClientControl::Request(DownloadRequest { offer_id, ids }))
        }
        _ => None,
    }
}

/// One entry of a `file-batch` file list: all five fields, nothing else.
fn announced_file(entry: &serde_json::Value) -> Option<AnnouncedFile> {
    let entry = entry.as_object()?;
    if entry
        .keys()
        .any(|key| !["name", "size", "mime", "mtimeMs", "sha256"].contains(&key.as_str()))
    {
        return None;
    }
    let name = entry.get("name")?.as_str()?.to_owned();
    let size = entry.get("size")?.as_u64()?;
    let mime = entry.get("mime")?.as_str()?.to_owned();
    let mtime_ms = entry.get("mtimeMs")?.as_u64()?;
    let sha256 = parse_sha256_hex(entry.get("sha256")?.as_str()?)?;
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || size > MAX_FILE_BYTES
        || mime.len() > MAX_MIME_BYTES
    {
        return None;
    }
    Some(AnnouncedFile {
        name,
        size,
        mime,
        mtime_ms,
        sha256,
    })
}

/// Host -> client `file-batch-ack`: the batch was admitted (`error` absent) or refused.
pub fn file_batch_ack_message(batch_id: &str, error: Option<&str>) -> String {
    let mut message = serde_json::json!({
        "type": "file-batch-ack",
        "protocol": PROTOCOL_VERSION,
        "batchId": batch_id,
        "ok": error.is_none(),
    });
    if let Some(error) = error {
        message["error"] = error.into();
    }
    message.to_string()
}

/// Signaling `file-offer` (host -> client on `/ws`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileOfferItem {
    pub id: u32,
    pub name: String,
    pub size: u64,
    pub mime: String,
    #[serde(rename = "mtimeMs")]
    pub mtime_ms: u64,
    /// SHA-256 of the file, 64 lowercase hex digits. The viewer verifies the download against it.
    pub sha256: String,
}

pub fn file_offer_message(offer_id: &str, items: &[FileOfferItem], source: &str) -> String {
    serde_json::json!({
        "type": "file-offer",
        "protocol": PROTOCOL_VERSION,
        "offerId": offer_id,
        "items": items,
        "source": source,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../fixtures/file-transfer-contract.json")).unwrap()
    }

    #[test]
    fn fixture_sealed_control_messages() {
        let contract = fixture();
        let signaling = &contract["signaling"];
        for case in signaling["fileBatch"]["cases"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let parsed = matches!(parse_client_control(text), Some(ClientControl::Batch(_)));
            assert_eq!(parsed, case["accepted"].as_bool().unwrap(), "{text}");
        }
        for case in signaling["fileBatchCancel"]["cases"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let parsed = matches!(parse_client_control(text), Some(ClientControl::Cancel(_)));
            assert_eq!(parsed, case["accepted"].as_bool().unwrap(), "{text}");
        }
        for case in signaling["fileRequest"]["cases"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let parsed = matches!(parse_client_control(text), Some(ClientControl::Request(_)));
            assert_eq!(parsed, case["accepted"].as_bool().unwrap(), "{text}");
        }
        for case in signaling["fileBatchAck"]["cases"].as_array().unwrap() {
            let wire: Value = serde_json::from_str(case["text"].as_str().unwrap()).unwrap();
            let ours =
                file_batch_ack_message(wire["batchId"].as_str().unwrap(), wire["error"].as_str());
            assert_eq!(serde_json::from_str::<Value>(&ours).unwrap(), wire);
        }
    }

    #[test]
    fn fixture_signaling_offers_parse() {
        let contract = fixture();
        for case in contract["signaling"]["fileOffer"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            assert_eq!(
                valid_file_offer_text(text),
                case["accepted"].as_bool().unwrap(),
                "{text}"
            );
        }
        let items = [FileOfferItem {
            id: 1,
            name: "report.pdf".into(),
            size: 1234,
            mime: "application/pdf".into(),
            mtime_ms: 1_700_000_000_000,
            sha256: "a".repeat(64),
        }];
        let built = file_offer_message(&test_id(7), &items, "clipboard");
        assert!(valid_file_offer_text(&built));
        assert_eq!(
            serde_json::from_str::<Value>(&built).unwrap(),
            serde_json::from_str::<Value>(
                contract["signaling"]["fileOffer"][0]["text"]
                    .as_str()
                    .unwrap()
            )
            .unwrap()
        );
    }

    /// Mirrors `valid_file_meta` in relay-rs/core/src/protocol.rs.
    fn valid_file_offer_text(text: &str) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        let Some(items) = value["items"].as_array() else {
            return false;
        };
        value["type"] == "file-offer"
            && value["protocol"] == 3
            && value["offerId"].as_str().is_some_and(valid_id128)
            && matches!(value["source"].as_str(), Some("clipboard" | "sendto"))
            && items.len() <= MAX_BATCH_FILES as usize
            && items.iter().all(|item| {
                item["id"].as_u64().is_some_and(|id| id <= u32::MAX as u64)
                    && item["name"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty() && name.len() <= MAX_NAME_BYTES)
                    && item["size"]
                        .as_u64()
                        .is_some_and(|size| size <= MAX_FILE_BYTES)
                    && item["mime"]
                        .as_str()
                        .is_some_and(|mime| mime.len() <= MAX_MIME_BYTES)
                    && item["mtimeMs"].as_u64().is_some()
                    && item["sha256"].as_str().is_some_and(|hash| {
                        hash.len() == SHA256_BYTES * 2
                            && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
            })
    }
}
