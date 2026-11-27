//! The admin side of the relay: the PC listing and deleting a PC.

use super::*;

impl Relay {
    /// The listing for the admin route: every remembered PC, sorted by id.
    pub fn list_pcs(&self) -> Result<Vec<PcListing>, AdminError> {
        if !self.healthy {
            return Err(AdminError::Unavailable);
        }
        Ok(self.listing())
    }

    /// The same rows without the health check: what an adapter that keeps one PC per object reports
    /// to its directory.
    pub fn listing(&self) -> Vec<PcListing> {
        self.pcs
            .records
            .iter()
            .map(|(id, record)| PcListing {
                id: id.clone(),
                public: util::b64url(&record.public),
                first_seen: record.first_seen,
                last_seen: record.last_seen,
                online: self.host_online(id),
                admitted: record.admitted.len(),
            })
            .collect()
    }

    /// Forgets a PC. Persists the table first (the first action), then closes
    /// every socket of it (host and viewer) 1008 "peer removed".
    /// An id that is not a device id, or a valid id with no registered PC, is
    /// `AdminError::NotFound` and nothing is persisted.
    pub fn delete_pc(&mut self, id: &str) -> Result<Vec<Action>, AdminError> {
        if !self.healthy {
            return Err(AdminError::Unavailable);
        }
        let pc = util::canonical_pc(id).ok_or(AdminError::NotFound)?;
        if self.pcs.records.remove(&pc).is_none() {
            return Err(AdminError::NotFound);
        }
        let mut out = vec![Action::PersistPcs(self.pcs.clone())];
        // Every socket of the PC, host and viewer alike: a viewer that stayed would keep a session
        // (and its media and file sockets) with a PC the admin has just removed.
        let sockets: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.pc == pc)
            .map(|(id, _)| *id)
            .collect();
        for id in &sockets {
            self.close(*id, 1008, reasons::PEER_REMOVED, "admin.pc-removed", &mut out);
        }
        for id in sockets {
            self.forget(id, &mut out);
        }
        Ok(out)
    }
}
