//! Which Durable Object serves a PC, and the directory of registered PCs.
//!
//! The Cloudflare Worker gives every PC its own object for session traffic (signalling, media,
//! control and file sockets), so streams for different PCs never share an object, a connection cap
//! or a CPU budget. Anything that needs a view across PCs lives in one small directory object: the
//! presence query, the admin listing and the cap on registered PCs. Each PC's object tells the
//! directory when its record or its host's presence changes ([`PcReport`]); the directory keeps one
//! [`DirEntry`] per PC. This module is the pure part of both: the object names and the registry.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::DeviceId;
use serde::{Deserialize, Serialize};

use crate::relay::{MAX_PCS, PcListing};

/// A report older than this no longer counts as "online". A PC's object repeats its report while
/// its host is connected (every [`REPORT_EVERY_MS`]), so an object that vanished without a close
/// event cannot leave a PC online for longer than this.
pub const ONLINE_TTL_MS: u64 = 10 * 60_000;
/// How often a PC's object repeats the online report while its host is connected.
pub const REPORT_EVERY_MS: u64 = 4 * 60_000;
/// The object that takes every upgrade without a usable `pc` (it answers the refusal, or the
/// token-gated version probe). No PC is ever kept there.
pub const NO_PC_OBJECT: &str = "pc-none";

/// The name of the object a session socket belongs to, from the upgrade's `pc` query value. The
/// same PC always gets the same name (any spelling of its id), another PC another name. A missing
/// or malformed value gets [`NO_PC_OBJECT`].
pub fn session_object_name(pc: Option<&str>) -> String {
    match pc.and_then(|raw| DeviceId::parse(raw).ok()) {
        Some(id) => format!("pc-{id}"),
        None => NO_PC_OBJECT.to_owned(),
    }
}

/// What a PC's object tells the directory. Sent when the PC registers or its record changes, when
/// its host goes online or offline, and every [`REPORT_EVERY_MS`] while the host is online.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcReport {
    /// Canonical device id.
    pub id: String,
    /// Unpadded base64url of the 32-byte public key.
    pub public: String,
    pub first_seen: u64,
    pub last_seen: u64,
    /// How many viewers the host has admitted. The ids stay in the PC's object.
    pub admitted: usize,
    /// True while the host's signalling socket is open.
    pub online: bool,
}

impl PcReport {
    /// True when the id is canonical, the key is a canonical 32-byte value and the id is the key's
    /// hash, the same rules the PC table follows.
    pub fn is_valid(&self) -> bool {
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(&self.public) else {
            return false;
        };
        let Ok(public) = <[u8; 32]>::try_from(bytes) else {
            return false;
        };
        URL_SAFE_NO_PAD.encode(public) == self.public
            && DeviceId::parse(&self.id).is_ok_and(|id| {
                id.to_string() == self.id && DeviceId::from_public_key(&public) == id
            })
    }
}

/// One registered PC as the directory keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirEntry {
    pub public: String,
    pub first_seen: u64,
    pub last_seen: u64,
    pub admitted: usize,
    /// When the PC's object last reported the host online. None while offline.
    pub online_at: Option<u64>,
}

/// A new PC was refused: the directory holds [`MAX_PCS`] already.
#[derive(Debug, PartialEq, Eq)]
pub struct CapReached;

/// The registered PCs, keyed by canonical device id.
#[derive(Debug, Default)]
pub struct Directory {
    entries: BTreeMap<String, DirEntry>,
}

impl Directory {
    pub fn new(entries: BTreeMap<String, DirEntry>) -> Self {
        Self { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Applies a report and returns the entry to store. A PC the directory does not know is
    /// refused once [`MAX_PCS`] are registered.
    pub fn report(&mut self, report: &PcReport, now_ms: u64) -> Result<DirEntry, CapReached> {
        if !self.entries.contains_key(&report.id) && self.entries.len() >= MAX_PCS {
            return Err(CapReached);
        }
        let entry = DirEntry {
            public: report.public.clone(),
            first_seen: report.first_seen,
            last_seen: report.last_seen,
            admitted: report.admitted,
            online_at: report.online.then_some(now_ms),
        };
        self.entries.insert(report.id.clone(), entry.clone());
        Ok(entry)
    }

    /// True when the PC is registered.
    pub fn is_registered(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// Forgets a PC. True when it was registered.
    pub fn remove(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    /// True while the PC's host is online: reported so within [`ONLINE_TTL_MS`].
    pub fn is_online(&self, id: &str, now_ms: u64) -> bool {
        self.entries
            .get(id)
            .and_then(|entry| entry.online_at)
            .is_some_and(|at| now_ms.saturating_sub(at) < ONLINE_TTL_MS)
    }

    /// The admin listing: every registered PC, sorted by id.
    pub fn listing(&self, now_ms: u64) -> Vec<PcListing> {
        self.entries
            .iter()
            .map(|(id, entry)| PcListing {
                id: id.clone(),
                public: entry.public.clone(),
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                online: self.is_online(id, now_ms),
                admitted: entry.admitted,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(seed: u8) -> String {
        DeviceId::from_public_key(&[seed; 32]).to_string()
    }

    fn report(seed: u8, online: bool) -> PcReport {
        PcReport {
            id: id(seed),
            public: URL_SAFE_NO_PAD.encode([seed; 32]),
            first_seen: 10,
            last_seen: 20,
            admitted: 2,
            online,
        }
    }

    #[test]
    fn each_pc_gets_its_own_object_and_a_viewer_reaches_its_pcs() {
        let (a, b) = (id(1), id(2));
        let host_a = session_object_name(Some(&a));
        let viewer_a = session_object_name(Some(&a.to_ascii_lowercase()));
        let host_b = session_object_name(Some(&b));
        assert_eq!(
            host_a, viewer_a,
            "a viewer lands in its PC's object, whatever the spelling"
        );
        assert_ne!(host_a, host_b, "two PCs never share an object");
        assert_eq!(host_a, format!("pc-{a}"));
        assert_ne!(host_a, NO_PC_OBJECT);
        // No usable pc: the shared refusal object, which keeps no PC.
        assert_eq!(session_object_name(None), NO_PC_OBJECT);
        assert_eq!(session_object_name(Some("nope")), NO_PC_OBJECT);
        assert_eq!(session_object_name(Some("")), NO_PC_OBJECT);
    }

    #[test]
    fn a_report_must_match_the_key_it_names() {
        assert!(report(1, true).is_valid());
        let mut wrong = report(1, true);
        wrong.public = URL_SAFE_NO_PAD.encode([2u8; 32]);
        assert!(!wrong.is_valid());
        let mut short = report(1, true);
        short.public = URL_SAFE_NO_PAD.encode([1u8; 31]);
        assert!(!short.is_valid());
        let mut lower = report(1, true);
        lower.id = lower.id.to_ascii_lowercase();
        assert!(!lower.is_valid(), "the id must be canonical");
    }

    #[test]
    fn presence_follows_the_reports_and_lapses_without_them() {
        let mut dir = Directory::default();
        let a = id(1);
        assert!(!dir.is_online(&a, 1_000), "an unknown PC is offline");
        dir.report(&report(1, true), 1_000).unwrap();
        assert!(dir.is_online(&a, 1_000 + ONLINE_TTL_MS - 1));
        assert!(
            !dir.is_online(&a, 1_000 + ONLINE_TTL_MS),
            "a host that stopped reporting is no longer online"
        );
        dir.report(&report(1, true), 5_000).unwrap();
        assert!(
            dir.is_online(&a, 5_000 + ONLINE_TTL_MS - 1),
            "a repeat report renews it"
        );
        dir.report(&report(1, false), 6_000).unwrap();
        assert!(!dir.is_online(&a, 6_000));
        assert_eq!(dir.len(), 1, "a repeat report is the same PC");
    }

    #[test]
    fn listing_is_sorted_and_remove_forgets() {
        let mut dir = Directory::default();
        dir.report(&report(2, true), 100).unwrap();
        dir.report(&report(1, false), 100).unwrap();
        let rows = dir.listing(100);
        let mut ids = vec![id(1), id(2)];
        ids.sort();
        assert_eq!(rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), ids);
        let two = rows.iter().find(|r| r.id == id(2)).unwrap();
        assert!(two.online && two.admitted == 2 && two.first_seen == 10 && two.last_seen == 20);
        assert!(dir.remove(&id(2)));
        assert!(!dir.remove(&id(2)));
        assert_eq!(dir.listing(100).len(), 1);
    }

    #[test]
    fn an_unknown_pc_is_refused_at_the_cap_but_a_known_one_still_reports() {
        let entry = |n: usize| DirEntry {
            public: String::new(),
            first_seen: n as u64,
            last_seen: 0,
            admitted: 0,
            online_at: None,
        };
        let mut full: BTreeMap<String, DirEntry> =
            (0..MAX_PCS).map(|n| (format!("x{n}"), entry(n))).collect();
        full.insert(id(1), entry(0));
        full.remove("x0");
        let mut dir = Directory::new(full);
        assert_eq!(dir.len(), MAX_PCS);
        assert!(
            dir.report(&report(1, true), 1).is_ok(),
            "a known PC is not a new registration"
        );
        assert_eq!(dir.report(&report(2, true), 1), Err(CapReached));
    }
}
