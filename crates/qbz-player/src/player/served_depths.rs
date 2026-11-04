//! The bit depth Qobuz actually serves for a track, per requested tier.
//!
//! A tier is a ceiling: asking for Hi-Res on a track mastered at 16 bits gets
//! the 16-bit file. The cache gate only sees the copy's own header, so without
//! this it refused such a copy on every play and fetched the same file again —
//! a skip to a prefetched track took 3-14 s instead of 0.2 s (2026-11-04,
//! track 7529809, UltraHiRes).

use std::collections::HashMap;
use std::sync::Mutex;

use qbz_models::Quality;

#[derive(Default)]
pub(crate) struct ServedDepths {
    by_track: Mutex<HashMap<u64, (Quality, u32)>>,
}

impl ServedDepths {
    /// Record that asking for `tier` got a `bit_depth` file for `track_id`.
    /// The highest tier seen wins: it answers the most requests.
    pub(crate) fn note(&self, track_id: u64, tier: Quality, bit_depth: Option<u32>) {
        let Some(bit_depth) = bit_depth else {
            return;
        };
        if let Ok(mut by_track) = self.by_track.lock() {
            let keep = by_track
                .get(&track_id)
                .is_some_and(|(seen, _)| *seen > tier);
            if !keep {
                by_track.insert(track_id, (tier, bit_depth));
            }
        }
    }

    /// The bit depth a request for `requested` would get, if a request at that
    /// tier or above has already been answered.
    pub(crate) fn best_for(&self, track_id: u64, requested: Quality) -> Option<u32> {
        let by_track = self.by_track.lock().ok()?;
        let (tier, bit_depth) = by_track.get(&track_id)?;
        (*tier >= requested).then_some(*bit_depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_at_a_tier_covers_that_tier_and_below() {
        let depths = ServedDepths::default();
        depths.note(1, Quality::UltraHiRes, Some(16));
        assert_eq!(depths.best_for(1, Quality::UltraHiRes), Some(16));
        assert_eq!(depths.best_for(1, Quality::HiRes), Some(16));
    }

    #[test]
    fn an_answer_at_a_lower_tier_says_nothing_about_a_higher_one() {
        let depths = ServedDepths::default();
        depths.note(1, Quality::Lossless, Some(16));
        assert_eq!(depths.best_for(1, Quality::HiRes), None);
    }

    #[test]
    fn a_lower_tier_answer_does_not_replace_a_higher_one() {
        let depths = ServedDepths::default();
        depths.note(1, Quality::UltraHiRes, Some(24));
        depths.note(1, Quality::Lossless, Some(16));
        assert_eq!(depths.best_for(1, Quality::UltraHiRes), Some(24));
    }

    #[test]
    fn an_unknown_depth_records_nothing() {
        let depths = ServedDepths::default();
        depths.note(1, Quality::UltraHiRes, None);
        assert_eq!(depths.best_for(1, Quality::UltraHiRes), None);
    }
}
