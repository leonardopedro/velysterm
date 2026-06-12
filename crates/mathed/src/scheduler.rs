//! Foot-style two-tier damage queue scheduler.
//!
//! Adapts the foot terminal's render scheduler concept to ms-scale
//! Typst eval. Content (block re-transforms) is batched with a lower
//! bound (`LOWER_S`) after the last keystroke and an upper bound
//! (`UPPER_S`) on staleness during a burst. Reveal changes (caret,
//! selection) fire immediately on the next call.

use std::collections::HashSet;

use bevy::prelude::*;
use mathed_core::blocks::BlockId;

/// Batch window after last keystroke (seconds).
pub const LOWER_S: f64 = 0.025;
/// Maximum staleness during a burst (seconds).
pub const UPPER_S: f64 = 0.100;
/// Maximum blocks to process per fire.
pub const MAX_BLOCKS_PER_FIRE: usize = 4;

/// Two-tier damage queue scheduler for block re-transforms and
/// reveal state updates.
#[derive(Resource, Default)]
pub struct Scheduler {
    dirty: HashSet<BlockId>,
    reveal_dirty: bool,
    pub doc_changed: bool,
    first_damage: Option<f64>,
    deadline: Option<f64>,
}

/// A set of work items to process in one sync pass.
pub struct FireSet {
    pub blocks: Vec<BlockId>,
    pub reveal: bool,
}

impl Scheduler {
    /// Note that blocks need re-transforming. Arms the timers.
    pub fn note_blocks(
        &mut self,
        ids: impl IntoIterator<Item = BlockId>,
        now: f64,
    ) {
        self.dirty.extend(ids);
        self.deadline = Some(now + LOWER_S);
        self.first_damage.get_or_insert(now);
    }

    /// Note that reveal state changed (caret, selection, show_hidden).
    pub fn note_reveal(&mut self) {
        self.reveal_dirty = true;
    }

    /// Try to fire. Returns `Some(FireSet)` when work should be done.
    ///
    /// - Content fires when dirty blocks exist and either the deadline
    ///   or the upper staleness bound has passed.
    /// - Reveal fires unconditionally on the next call.
    /// - Up to `MAX_BLOCKS_PER_FIRE` blocks per fire; remaining blocks
    ///   re-arm the deadline.
    pub fn take(&mut self, now: f64) -> Option<FireSet> {
        let content_ready = !self.dirty.is_empty()
            && (self.deadline.map_or(false, |d| now >= d)
                || self
                    .first_damage
                    .map_or(false, |t| now >= t + UPPER_S));

        let reveal_ready = self.reveal_dirty;

        if !content_ready && !reveal_ready {
            return None;
        }

        // Collect up to MAX_BLOCKS_PER_FIRE block ids.
        let mut blocks: Vec<BlockId> = Vec::new();
        if content_ready {
            let take_count =
                MAX_BLOCKS_PER_FIRE.min(self.dirty.len());
            blocks
                .extend(self.dirty.iter().copied().take(take_count));
            for id in &blocks {
                self.dirty.remove(id);
            }
            if self.dirty.is_empty() {
                self.deadline = None;
                self.first_damage = None;
            } else {
                // Re-arm deadline for remaining blocks.
                self.deadline = Some(now + LOWER_S);
            }
        }

        let reveal = reveal_ready;
        self.reveal_dirty = false;

        Some(FireSet { blocks, reveal })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> BlockId {
        BlockId(n)
    }

    #[test]
    fn single_edit_fires_at_lower() {
        let mut s = Scheduler::default();
        s.note_blocks([id(1)], 0.0);
        // Too early.
        assert!(s.take(0.01).is_none());
        // At LOWER_S.
        let fire = s.take(LOWER_S).unwrap();
        assert_eq!(fire.blocks, vec![id(1)]);
        assert!(!fire.reveal);
    }

    #[test]
    fn burst_fires_at_upper() {
        let mut s = Scheduler::default();
        // Repeated edits at 10ms intervals.
        for i in 0..10 {
            let t = i as f64 * 0.01;
            s.note_blocks([id(1)], t);
        }
        // Should fire at first_damage + UPPER_S.
        let fire = s.take(UPPER_S).unwrap();
        assert_eq!(fire.blocks, vec![id(1)]);
    }

    #[test]
    fn reveal_fires_immediately() {
        let mut s = Scheduler::default();
        s.note_reveal();
        let fire = s.take(0.0).unwrap();
        assert!(fire.reveal);
        assert!(fire.blocks.is_empty());
    }

    #[test]
    fn budget_splits_fires() {
        let mut s = Scheduler::default();
        // 6 dirty blocks.
        s.note_blocks(
            [id(1), id(2), id(3), id(4), id(5), id(6)],
            0.0,
        );
        // First fire: 4 blocks.
        let fire = s.take(LOWER_S).unwrap();
        assert_eq!(fire.blocks.len(), 4);
        // Remaining 2 blocks need a new deadline.
        assert!(s.take(LOWER_S + 0.001).is_none());
        let fire2 = s.take(LOWER_S * 2.0).unwrap();
        assert_eq!(fire2.blocks.len(), 2);
    }

    #[test]
    fn no_work_no_fire() {
        let mut s = Scheduler::default();
        assert!(s.take(1.0).is_none());
    }
}
