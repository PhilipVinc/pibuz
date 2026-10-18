// crates/pibuz/src/mute.rs — the daemon's ONE mute, whoever changes it.
//
// The box (`POST /api/playback/volume {"mute": ...}`) and the controller
// (`MuteVolume`, through `DaemonRendererEngine::set_muted`) both mute here, and
// the volume publisher reads it to tell the controller. They used to be two
// states: the box's lived in `DaemonShared`, the controller's only in the
// player's level. So a box-side mute reached the phone as a level of 0, its
// slider dragged to the bottom and the unmute never reported (issue #4), and
// the box could not unmute a mute made on the phone at all.
//
// Muting stashes the level and sets the player to 0. The level everyone
// REPORTS stays the stashed one (`nominal`): `vol 80%` reads 80% through a
// mute, and the controller's slider does not move while its mute icon does.

use std::sync::Mutex;

use crate::state::DaemonShared;

/// Restored when the stashed level is 0, so a mute taken at 0 still unmutes to
/// something audible.
const FALLBACK_LEVEL: f32 = 0.7;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Mute {
    muted: bool,
    /// The level an unmute restores. Meaningful only while `muted`.
    premute: f32,
}

impl Default for Mute {
    fn default() -> Self {
        Self {
            muted: false,
            premute: 1.0,
        }
    }
}

impl Mute {
    pub(crate) fn is_muted(&self) -> bool {
        self.muted
    }

    /// The level to report, given the player's live one: the stashed level
    /// while muted, when the player itself sits at 0.
    pub(crate) fn nominal(&self, live: f32) -> f32 {
        if self.muted {
            self.premute
        } else {
            live
        }
    }

    /// Mute or unmute. Returns the level the player must be set to, or `None`
    /// when it is already in the requested state.
    pub(crate) fn set(&mut self, on: bool, live: f32) -> Option<f32> {
        match (on, self.muted) {
            (true, false) => {
                self.premute = if live > 0.0 { live } else { FALLBACK_LEVEL };
                self.muted = true;
                Some(0.0)
            }
            (false, true) => {
                self.muted = false;
                Some(if self.premute > 0.0 {
                    self.premute
                } else {
                    FALLBACK_LEVEL
                })
            }
            _ => None,
        }
    }

    /// A listener setting a level, on the box or on the controller. A non-zero
    /// one ends a mute — the same rule both sides had before — and the player
    /// goes to that level either way.
    pub(crate) fn take_level(&mut self, level: f32) -> Option<f32> {
        if level > 0.0 {
            self.muted = false;
        }
        Some(level)
    }

    /// A level nobody chose just now: `qconnect.initial_volume` at a join.
    /// While muted it becomes what the unmute restores and the player stays
    /// silent — a cast must not unmute the box.
    pub(crate) fn set_level_quietly(&mut self, level: f32) -> Option<f32> {
        if self.muted {
            self.premute = level;
            None
        } else {
            Some(level)
        }
    }
}

/// Apply `change` to the daemon's mute and set the player to the level it asks
/// for, holding the daemon lock across both: a mute from the box and an unmute
/// from the controller racing each other must not leave the flag saying one
/// thing and the player the other. Returns the mute as it now stands.
///
/// On a failed `set_volume` the flag has still moved. That is the old
/// behaviour of the box's mute, kept: the player's command channel is dead at
/// that point, and nothing it reports means much.
pub(crate) fn change(
    shared: &Mutex<DaemonShared>,
    set_volume: impl FnOnce(f32) -> Result<(), String>,
    change: impl FnOnce(&mut Mute) -> Option<f32>,
) -> Result<Mute, String> {
    let mut guard = shared
        .lock()
        .map_err(|_| "daemon state lock poisoned".to_string())?;
    if let Some(level) = change(&mut guard.mute) {
        set_volume(level)?;
    }
    Ok(guard.mute)
}

/// The mute as it stands, or unmuted if the lock is poisoned.
pub(crate) fn current(shared: &Mutex<DaemonShared>) -> Mute {
    shared.lock().map(|s| s.mute).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mute_round_trips_the_level_and_reports_it_throughout() {
        let mut mute = Mute::default();
        assert_eq!(mute.set(true, 0.4), Some(0.0));
        assert!(mute.is_muted());
        assert_eq!(mute.nominal(0.0), 0.4, "the reported level holds");
        assert_eq!(mute.set(false, 0.0), Some(0.4));
        assert!(!mute.is_muted());
        assert_eq!(mute.nominal(0.4), 0.4);
    }

    #[test]
    fn asking_for_the_state_already_held_changes_nothing() {
        let mut mute = Mute::default();
        assert_eq!(mute.set(false, 0.4), None);
        mute.set(true, 0.4);
        assert_eq!(mute.set(true, 0.0), None);
        assert_eq!(mute.nominal(0.0), 0.4, "a second mute must not stash 0");
    }

    #[test]
    fn a_mute_taken_at_zero_unmutes_to_something_audible() {
        let mut mute = Mute::default();
        mute.set(true, 0.0);
        assert_eq!(mute.set(false, 0.0), Some(FALLBACK_LEVEL));
    }

    #[test]
    fn a_listeners_level_ends_a_mute_and_zero_does_not() {
        let mut mute = Mute::default();
        mute.set(true, 0.4);
        assert_eq!(mute.take_level(0.0), Some(0.0));
        assert!(mute.is_muted());
        assert_eq!(mute.take_level(0.6), Some(0.6));
        assert!(!mute.is_muted());
    }

    #[test]
    fn a_join_time_level_does_not_unmute() {
        let mut mute = Mute::default();
        mute.set(true, 0.4);
        assert_eq!(mute.set_level_quietly(0.3), None, "the player stays silent");
        assert_eq!(mute.nominal(0.0), 0.3);
        assert_eq!(mute.set(false, 0.0), Some(0.3), "the unmute restores it");

        assert_eq!(mute.set_level_quietly(0.25), Some(0.25), "unmuted: applied");
    }
}
