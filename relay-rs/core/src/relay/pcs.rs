//! The PC table: the remembered host identities.

use std::collections::{BTreeMap, BTreeSet};

/// Go's maxAuthRecords: an unknown PC is refused once the table holds this many.
pub const MAX_PCS: usize = 4096;

/// One remembered PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcRecord {
    /// The PC's Ed25519 public key.
    pub public: [u8; 32],
    /// Unix milliseconds of the first registration.
    pub first_seen: u64,
    /// Unix milliseconds of the most recent registration.
    pub last_seen: u64,
    /// Canonical IDs of the viewers the host has admitted. Each admit message
    /// replaces the whole set.
    pub admitted: BTreeSet<String>,
    /// The host's remotePairing flag. Stored in M2 and not used yet.
    pub remote_pairing: bool,
}

/// The PC table, keyed by canonical device id. The adapter loads it at start
/// and receives a snapshot in every `Action::PersistPcs`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PcTable {
    /// Records keyed by canonical device id (`XXXX-XXXX-XXXX-XXXX`).
    pub records: BTreeMap<String, PcRecord>,
}

impl PcTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of remembered PCs.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when no PC is remembered.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}
