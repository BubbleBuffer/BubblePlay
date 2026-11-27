//! Matching a download REQUEST on the file socket with the current offer and with the viewer's
//! sealed `file-request` (see `docs/architecture/file-transfer.md`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{lock, OfferedFile, Shared};
use crate::file_transfer::RequestPayload;

/// What a download REQUEST resolves to.
pub(super) enum Resolved {
    Item(OfferedFile, u32, u32),
    Wait,
    Unauthorized,
    Unknown,
}

/// The sealed `file-request` may trail the REQUEST by `grace`; after that the REQUEST is refused.
pub(super) fn resolve(
    shared: &Mutex<Shared>,
    request: &RequestPayload,
    asked_at: Instant,
    grace: Duration,
) -> Resolved {
    let mut state = lock(shared);
    let Some(offer) = state
        .offer
        .as_ref()
        .filter(|offer| offer.id == request.offer_id)
    else {
        return Resolved::Unknown;
    };
    let Some(index) = offer.items.iter().position(|item| item.id == request.id) else {
        return Resolved::Unknown;
    };
    let (item, count) = (offer.items[index].clone(), offer.items.len() as u32);
    if state
        .auth
        .take_grant(&request.offer_id, request.id, Instant::now())
    {
        Resolved::Item(item, index as u32, count)
    } else if asked_at.elapsed() < grace {
        Resolved::Wait
    } else {
        Resolved::Unauthorized
    }
}
