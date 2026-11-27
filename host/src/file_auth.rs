//! Which file-socket packets the host may act on (pure logic, no sockets or files).
//!
//! The relay carries the file socket and is not trusted for integrity, so a packet there proves
//! nothing by itself. Every upload batch is announced over the sealed control channel
//! (`file-batch`) with each file's size and SHA-256 before the host accepts its OFFER, and every
//! download is asked for over the same channel (`file-request`) before the host serves its
//! REQUEST. See `docs/architecture/file-transfer.md`.

use std::time::{Duration, Instant};

use crate::file_transfer::{BatchAnnouncement, DropPoint, OfferMeta, Purpose, SHA256_BYTES};

/// An announced batch is dropped when this long passes without progress on it.
const BATCH_IDLE: Duration = Duration::from_secs(15 * 60);
/// A download grant is dropped when the REQUEST does not follow this soon.
const GRANT_TTL: Duration = Duration::from_secs(60);
/// Announced batches the host holds at once.
const MAX_BATCHES: usize = 8;
const MAX_GRANTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Progress {
    Pending,
    Started,
    Ended,
}

#[derive(Debug)]
struct AnnouncedBatch {
    announcement: BatchAnnouncement,
    progress: Vec<Progress>,
    deadline: Instant,
}

#[derive(Debug)]
struct Grant {
    offer_id: String,
    ids: Vec<u32>,
    deadline: Instant,
}

/// What the viewer announced and asked for over the sealed control channel.
#[derive(Debug, Default)]
pub struct Authorizations {
    batches: Vec<AnnouncedBatch>,
    grants: Vec<Grant>,
}

impl Authorizations {
    fn purge(&mut self, now: Instant) {
        self.batches.retain(|batch| batch.deadline > now);
        self.grants.retain(|grant| grant.deadline > now);
    }

    /// Forgets everything (the viewer changed).
    pub fn clear(&mut self) {
        self.batches.clear();
        self.grants.clear();
    }

    /// Admits an announced batch. Error codes are the `file-batch-ack` errors.
    pub fn announce(
        &mut self,
        announcement: BatchAnnouncement,
        now: Instant,
    ) -> Result<(), &'static str> {
        self.purge(now);
        if self
            .batches
            .iter()
            .any(|batch| batch.announcement.batch_id == announcement.batch_id)
        {
            return Err("duplicate-batch");
        }
        if self.batches.len() >= MAX_BATCHES {
            return Err("busy");
        }
        let progress = vec![Progress::Pending; announcement.files.len()];
        self.batches.push(AnnouncedBatch {
            announcement,
            progress,
            deadline: now + BATCH_IDLE,
        });
        Ok(())
    }

    /// Checks an OFFER against the announced batches and marks that file started. Returns the
    /// SHA-256 the file must have. A file can start once: replaying an offer fails.
    pub fn begin_file(
        &mut self,
        meta: &OfferMeta,
        now: Instant,
    ) -> Result<[u8; SHA256_BYTES], &'static str> {
        self.purge(now);
        let batch = self
            .batches
            .iter_mut()
            .find(|batch| batch.announcement.batch_id == meta.batch_id)
            .ok_or("unannounced")?;
        let announcement = &batch.announcement;
        let index = meta.batch_index as usize;
        let file = announcement.files.get(index).ok_or("mismatch")?;
        let same_drop: Option<DropPoint> = announcement.drop;
        if announcement.purpose != meta.purpose
            || announcement.files.len() != meta.batch_count as usize
            || same_drop != meta.drop
            || file.name != meta.name
            || file.size != meta.size
        {
            return Err("mismatch");
        }
        if batch.progress[index] != Progress::Pending {
            return Err("replayed");
        }
        batch.progress[index] = Progress::Started;
        batch.deadline = now + BATCH_IDLE;
        Ok(file.sha256)
    }

    /// The file started by `begin_file` finished (verified or not). Its batch is forgotten once
    /// no file of it can start any more.
    pub fn end_file(&mut self, batch_id: &str, index: usize, now: Instant) {
        if let Some(batch) = self
            .batches
            .iter_mut()
            .find(|batch| batch.announcement.batch_id == batch_id)
        {
            if let Some(slot) = batch.progress.get_mut(index) {
                *slot = Progress::Ended;
            }
            batch.deadline = now + BATCH_IDLE;
        }
        self.batches
            .retain(|batch| batch.progress.iter().any(|p| *p != Progress::Ended));
    }

    /// Drops what an abandoned batch still held (a cancelled drop, a closed socket).
    pub fn forget_batch(&mut self, batch_id: &str) {
        self.batches
            .retain(|batch| batch.announcement.batch_id != batch_id);
    }

    /// The viewer asked for these items of `offer_id`.
    pub fn grant(&mut self, offer_id: &str, ids: &[u32], now: Instant) {
        self.purge(now);
        if self.grants.len() >= MAX_GRANTS {
            self.grants.remove(0);
        }
        self.grants.push(Grant {
            offer_id: offer_id.to_owned(),
            ids: ids.to_vec(),
            deadline: now + GRANT_TTL,
        });
    }

    /// Consumes one grant for `(offer_id, id)`; false when the viewer did not ask for it (or it
    /// was served already).
    pub fn take_grant(&mut self, offer_id: &str, id: u32, now: Instant) -> bool {
        self.purge(now);
        for grant in &mut self.grants {
            if grant.offer_id == offer_id {
                if let Some(at) = grant.ids.iter().position(|candidate| *candidate == id) {
                    grant.ids.remove(at);
                    self.grants.retain(|grant| !grant.ids.is_empty());
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transfer::AnnouncedFile;

    fn id(n: u32) -> String {
        format!("{n:032x}")
    }

    fn announcement(batch: u32, sizes: &[u64]) -> BatchAnnouncement {
        BatchAnnouncement {
            batch_id: id(batch),
            purpose: Purpose::Paste,
            drop: None,
            files: sizes
                .iter()
                .enumerate()
                .map(|(i, size)| AnnouncedFile {
                    name: format!("f{i}.bin"),
                    size: *size,
                    sha256: [i as u8; SHA256_BYTES],
                })
                .collect(),
        }
    }

    fn offer(batch: u32, index: u32, count: u32, size: u64) -> OfferMeta {
        OfferMeta {
            name: format!("f{index}.bin"),
            size,
            mime: String::new(),
            mtime_ms: 0,
            purpose: Purpose::Paste,
            batch_id: id(batch),
            batch_index: index,
            batch_count: count,
            drop: None,
        }
    }

    #[test]
    fn only_an_announced_matching_offer_starts_once() {
        let now = Instant::now();
        let mut auth = Authorizations::default();
        assert_eq!(auth.begin_file(&offer(1, 0, 1, 5), now), Err("unannounced"));
        auth.announce(announcement(1, &[5, 7]), now).unwrap();
        assert_eq!(
            auth.announce(announcement(1, &[5]), now),
            Err("duplicate-batch")
        );
        assert_eq!(auth.begin_file(&offer(1, 0, 2, 6), now), Err("mismatch"));
        assert_eq!(auth.begin_file(&offer(1, 0, 3, 5), now), Err("mismatch"));
        let mut renamed = offer(1, 0, 2, 5);
        renamed.name = "other.bin".into();
        assert_eq!(auth.begin_file(&renamed, now), Err("mismatch"));
        assert_eq!(auth.begin_file(&offer(1, 0, 2, 5), now), Ok([0; 32]));
        assert_eq!(auth.begin_file(&offer(1, 0, 2, 5), now), Err("replayed"));
        assert_eq!(auth.begin_file(&offer(1, 1, 2, 7), now), Ok([1; 32]));
        auth.end_file(&id(1), 0, now);
        auth.end_file(&id(1), 1, now);
        assert_eq!(
            auth.begin_file(&offer(1, 1, 2, 7), now),
            Err("unannounced"),
            "a finished batch is gone"
        );
    }

    #[test]
    fn announcements_expire_and_cleared_state_is_forgotten() {
        let now = Instant::now();
        let mut auth = Authorizations::default();
        auth.announce(announcement(1, &[5]), now).unwrap();
        let later = now + BATCH_IDLE + Duration::from_secs(1);
        assert_eq!(
            auth.begin_file(&offer(1, 0, 1, 5), later),
            Err("unannounced")
        );
        auth.announce(announcement(2, &[5]), now).unwrap();
        auth.clear();
        assert_eq!(auth.begin_file(&offer(2, 0, 1, 5), now), Err("unannounced"));
    }

    #[test]
    fn a_download_grant_serves_one_request_for_one_item() {
        let now = Instant::now();
        let mut auth = Authorizations::default();
        assert!(!auth.take_grant(&id(9), 1, now));
        auth.grant(&id(9), &[1, 2], now);
        assert!(!auth.take_grant(&id(8), 1, now), "other offer");
        assert!(!auth.take_grant(&id(9), 3, now), "other item");
        assert!(auth.take_grant(&id(9), 1, now));
        assert!(!auth.take_grant(&id(9), 1, now), "used once");
        assert!(auth.take_grant(&id(9), 2, now));
        auth.grant(&id(9), &[1], now);
        assert!(!auth.take_grant(&id(9), 1, now + GRANT_TTL + Duration::from_secs(1)));
    }
}
