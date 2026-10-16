//! Decoder-chain identity, independent of the network receive gate. Epoch,
//! codec and dimensions identify a transform; a newly accepted keyframe after
//! a discontinuity must never inherit queued output from the old chain.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputTag {
    pub epoch: u64,
    pub generation: u32,
    pub sequence: u32,
    pub codec: u8,
    pub width: u16,
    pub height: u16,
}

#[derive(Default)]
pub struct OrderTracker {
    last: Option<InputTag>,
    minimum_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainAction {
    Stale,
    Continue,
    Recreate,
}

impl OrderTracker {
    pub fn reset(&mut self, epoch: u64) {
        self.minimum_epoch = self.minimum_epoch.max(epoch);
        self.last = None;
    }

    pub fn accept(&mut self, next: InputTag) -> ChainAction {
        if next.epoch < self.minimum_epoch {
            return ChainAction::Stale;
        }
        let action = match self.last {
            Some(last)
                if last.epoch == next.epoch
                    && last.generation == next.generation
                    && last.codec == next.codec
                    && last.width == next.width
                    && last.height == next.height
                    && last.sequence.wrapping_add(1) == next.sequence =>
            {
                ChainAction::Continue
            }
            _ => ChainAction::Recreate,
        };
        self.minimum_epoch = self.minimum_epoch.max(next.epoch);
        self.last = Some(next);
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(sequence: u32) -> InputTag {
        InputTag {
            epoch: 1,
            generation: 7,
            sequence,
            codec: 2,
            width: 1920,
            height: 1080,
        }
    }
    #[test]
    fn sequential_idrs_keep_the_decoder_but_discontinuities_replace_it() {
        let mut tracker = OrderTracker::default();
        assert_eq!(tracker.accept(frame(0)), ChainAction::Recreate);
        assert_eq!(tracker.accept(frame(1)), ChainAction::Continue);
        assert_eq!(tracker.accept(frame(3)), ChainAction::Recreate);
        assert_eq!(
            tracker.accept(InputTag {
                generation: 8,
                ..frame(4)
            }),
            ChainAction::Recreate
        );
        assert_eq!(
            tracker.accept(InputTag {
                codec: 1,
                ..frame(5)
            }),
            ChainAction::Recreate
        );
        assert_eq!(
            tracker.accept(InputTag {
                width: 1280,
                ..frame(6)
            }),
            ChainAction::Recreate
        );
    }
    #[test]
    fn reset_drops_prior_epochs_and_same_epoch_decoder_errors() {
        let mut tracker = OrderTracker::default();
        tracker.accept(frame(4));
        tracker.reset(2);
        assert_eq!(tracker.accept(frame(5)), ChainAction::Stale);
        assert_eq!(
            tracker.accept(InputTag {
                epoch: 2,
                ..frame(6)
            }),
            ChainAction::Recreate
        );
        tracker.reset(2);
        assert_eq!(
            tracker.accept(InputTag {
                epoch: 2,
                ..frame(7)
            }),
            ChainAction::Recreate
        );
        tracker.reset(1);
        assert_eq!(tracker.accept(frame(8)), ChainAction::Stale);
    }
    #[test]
    fn sequence_rollover_is_continuous() {
        let mut tracker = OrderTracker::default();
        tracker.accept(frame(u32::MAX));
        assert_eq!(tracker.accept(frame(0)), ChainAction::Continue);
    }
}
