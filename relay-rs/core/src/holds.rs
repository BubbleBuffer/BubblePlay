//! A cap on navigations held open for approval. Every held navigation keeps a request and a timer
//! alive until the gate decides, and anyone can send one, so the number is bounded in total and
//! per client prefix. A navigation over a limit is dropped at once, as an expired hold is.

use std::collections::BTreeMap;

use crate::gate::Prefix;

/// Navigations one client prefix may hold open.
pub const MAX_HELD_PER_PREFIX: usize = 4;

/// Navigations held open in total.
pub const MAX_HELD_TOTAL: usize = 64;

/// How many navigations are held open, in total and per client prefix.
#[derive(Debug, Default)]
pub struct HoldCounter {
    total: usize,
    by_prefix: BTreeMap<Prefix, usize>,
}

impl HoldCounter {
    /// Counts one more held navigation for `prefix`. False, and nothing counted, over a limit.
    pub fn acquire(&mut self, prefix: Prefix) -> bool {
        let held = self.by_prefix.get(&prefix).copied().unwrap_or(0);
        if self.total >= MAX_HELD_TOTAL || held >= MAX_HELD_PER_PREFIX {
            return false;
        }
        self.total += 1;
        self.by_prefix.insert(prefix, held + 1);
        true
    }

    /// Ends one held navigation for `prefix`.
    pub fn release(&mut self, prefix: Prefix) {
        let Some(held) = self.by_prefix.get_mut(&prefix) else {
            return;
        };
        *held -= 1;
        if *held == 0 {
            self.by_prefix.remove(&prefix);
        }
        self.total -= 1;
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn prefix(last: u8) -> Prefix {
        Prefix::of(IpAddr::V4(Ipv4Addr::new(198, 51, last, 1)))
    }

    #[test]
    fn a_prefix_and_the_total_are_capped_and_released() {
        let mut holds = HoldCounter::default();
        for _ in 0..MAX_HELD_PER_PREFIX {
            assert!(holds.acquire(prefix(0)));
        }
        assert!(!holds.acquire(prefix(0)), "one prefix cannot hold more");
        assert!(holds.acquire(prefix(1)), "another prefix is not affected");
        holds.release(prefix(0));
        assert!(holds.acquire(prefix(0)));
        for last in 2..=u8::MAX {
            if !holds.acquire(prefix(last)) {
                break;
            }
        }
        assert!(!holds.acquire(prefix(u8::MAX)), "the total is capped");
    }
}
