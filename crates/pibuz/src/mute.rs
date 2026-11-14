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

use crate::state::{DaemonShared, VolumeNews};

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

/// Who is changing the volume, which decides what the controller has to be
/// told ([`news_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// The box: the control API.
    Box,
    /// A level from the Connect controller (`SetVolume`).
    ControllerLevel,
    /// A mute from the Connect controller (`MuteVolume`).
    ControllerMute,
    /// The level a controller hands over when the box joins a session.
    Join,
}

/// Apply `change` to the daemon's mute and set the player to the level it asks
/// for, holding the daemon lock across both: a mute from the box and an unmute
/// from the controller racing each other must not leave the flag saying one
/// thing and the player the other. Returns the mute as it now stands, and
/// the level the player was set to, if `change` asked for one.
///
/// Every volume and mute change goes through here, so this is also where what
/// the controller has not been told is recorded (`DaemonShared::volume_news`)
/// and where the volume publisher and the events bridge are woken. Those were
/// a flag and a `notify_one` each caller had to remember, and the callers that
/// forgot are why a mute at level 0 never reached the phone, a level set to
/// the one it had left news to misfire on the next drag, and the join-time
/// volume reached `/api/events` late.
///
/// On a failed `set_volume` the flag has still moved. That is the old
/// behaviour of the box's mute, kept: the player's command channel is dead at
/// that point, and the error reaches the caller.
pub(crate) fn change(
    shared: &Mutex<DaemonShared>,
    source: Source,
    set_volume: impl FnOnce(f32) -> Result<(), String>,
    change: impl FnOnce(&mut Mute) -> Option<f32>,
) -> Result<(Mute, Option<f32>), String> {
    let mut guard = shared
        .lock()
        .map_err(|_| "daemon state lock poisoned".to_string())?;
    let before = guard.mute;
    let level = change(&mut guard.mute);
    if let Some(level) = level {
        set_volume(level)?;
    }
    let news = news_of(source, before, guard.mute, level);
    if news != VolumeNews::None {
        guard.volume_news = guard.volume_news.merge(news);
        guard.volume_news_wake.notify_one();
    }
    guard.events_edge.notify_one();
    Ok((guard.mute, level))
}

/// What a change tells the controller that it does not already know.
fn news_of(source: Source, before: Mute, after: Mute, level: Option<f32>) -> VolumeNews {
    let mute_moved = before.is_muted() != after.is_muted();
    match source {
        Source::Box if level.is_some() || mute_moved => VolumeNews::Local,
        // Its own level and mute it knows; the one thing it does not learn
        // from its echo is that its level ended a mute.
        Source::ControllerLevel if before.is_muted() && !after.is_muted() => VolumeNews::Unmute,
        _ => VolumeNews::None,
    }
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

    fn shared() -> Mutex<DaemonShared> {
        Mutex::new(DaemonShared::new())
    }

    const PLAYER: fn(f32) -> Result<(), String> = |_| Ok(());

    fn news(shared: &Mutex<DaemonShared>) -> VolumeNews {
        shared.lock().unwrap().volume_news
    }

    /// Whether `notify` holds a wake-up, consuming it.
    fn woken(notify: &tokio::sync::Notify) -> bool {
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.enable()
    }

    fn publisher_woken(shared: &Mutex<DaemonShared>) -> bool {
        let wake = std::sync::Arc::clone(&shared.lock().unwrap().volume_news_wake);
        woken(&wake)
    }

    fn events_woken(shared: &Mutex<DaemonShared>) -> bool {
        let edge = std::sync::Arc::clone(&shared.lock().unwrap().events_edge);
        woken(&edge)
    }

    /// The player is set to exactly the level the change asked for, and the
    /// caller is told it: `unmuted · vol 0%` was the API reporting the level
    /// from BEFORE the unmute.
    #[test]
    fn a_change_sets_the_player_and_says_what_it_set() {
        let shared = shared();
        let set = std::cell::Cell::new(None);
        let player = |level| {
            set.set(Some(level));
            Ok(())
        };

        let (mute, level) = change(&shared, Source::Box, player, |m| m.set(true, 0.26)).unwrap();
        assert!(mute.is_muted());
        assert_eq!((level, set.get()), (Some(0.0), Some(0.0)));

        let (mute, level) = change(&shared, Source::Box, player, |m| m.set(false, 0.0)).unwrap();
        assert!(!mute.is_muted());
        assert_eq!((level, set.get()), (Some(0.26), Some(0.26)));

        set.set(None);
        let (_, level) = change(&shared, Source::Box, player, |m| m.set(false, 0.26)).unwrap();
        assert_eq!(
            (level, set.get()),
            (None, None),
            "a no-op leaves the player alone"
        );
    }

    /// A controller's level that ends a mute is news to it, the unmute alone;
    /// one that does not end one is not news at all: it echoes its own level.
    #[test]
    fn a_controllers_level_reports_only_the_unmute_it_makes() {
        let shared = shared();
        change(&shared, Source::ControllerMute, PLAYER, |m| {
            m.set(true, 0.4)
        })
        .unwrap();
        assert_eq!(news(&shared), VolumeNews::None, "its own mute it knows");

        change(&shared, Source::ControllerLevel, PLAYER, |m| {
            m.take_level(0.0)
        })
        .unwrap();
        assert_eq!(
            news(&shared),
            VolumeNews::None,
            "a level of 0 leaves the mute on"
        );

        let (mute, level) = change(&shared, Source::ControllerLevel, PLAYER, |m| {
            m.take_level(0.6)
        })
        .unwrap();
        assert!(!mute.is_muted());
        assert_eq!(level, Some(0.6));
        assert_eq!(news(&shared), VolumeNews::Unmute);
        assert!(publisher_woken(&shared));

        shared.lock().unwrap().volume_news = VolumeNews::None;
        change(&shared, Source::ControllerLevel, PLAYER, |m| {
            m.take_level(0.5)
        })
        .unwrap();
        assert_eq!(news(&shared), VolumeNews::None, "unmuted already");
    }

    /// A mute on the box while the player is at 0 leaves the level where it
    /// was, so the bus's `VolumeChanged` — the only thing that used to run the
    /// publisher — never fired, and the phone never learned of the mute.
    #[test]
    fn a_box_mute_at_level_zero_wakes_the_publisher() {
        let shared = shared();
        change(&shared, Source::Box, PLAYER, |m| m.set(true, 0.0)).unwrap();
        assert_eq!(news(&shared), VolumeNews::Local);
        assert!(publisher_woken(&shared), "nothing else will run it");
    }

    /// A level set to the one it had moves nothing either. The news it left
    /// waited for the controller's next drag, which the publisher then took
    /// for a local change and sent back: the slider jumped under the finger.
    /// Woken at once, the publisher finds the controller already shows it and
    /// sends nothing, and the news is gone before any drag.
    #[test]
    fn a_level_set_to_the_one_it_had_is_not_left_waiting() {
        let shared = shared();
        change(&shared, Source::Box, PLAYER, |m| m.take_level(0.5)).unwrap();
        assert!(publisher_woken(&shared));
    }

    /// Every change wakes the events bridge, the join-time level included,
    /// which was the one caller that did not.
    #[test]
    fn every_change_wakes_the_events_bridge() {
        let shared = shared();
        change(&shared, Source::Join, PLAYER, |m| m.set_level_quietly(0.3)).unwrap();
        assert!(events_woken(&shared));
        assert_eq!(
            news(&shared),
            VolumeNews::None,
            "the controller handed it over"
        );
        assert!(!publisher_woken(&shared));

        change(&shared, Source::ControllerMute, PLAYER, |m| {
            m.set(true, 0.3)
        })
        .unwrap();
        assert!(events_woken(&shared));
    }

    /// News on top of news keeps the stronger: a box change after a
    /// controller's unmute is reported in full, and an unmute after a box
    /// change does not narrow it.
    #[test]
    fn news_keeps_the_stronger() {
        use VolumeNews::{Local, None, Unmute};
        assert_eq!(Unmute.merge(Local), Local);
        assert_eq!(Local.merge(Unmute), Local);
        assert_eq!(None.merge(Unmute), Unmute);
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
