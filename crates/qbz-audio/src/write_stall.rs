//! When a write that is waiting for room has waited long enough to call the
//! device dead.
//!
//! # The failure this is for
//!
//! A DAC whose clock STOPS — the PCM still Running, the hardware pointer no
//! longer moving — never frees any room in the ring. The ALSA write loop then
//! waits for room in 200 ms steps, finds none, and waits again, for ever: the
//! writer thread never returns from the write, nothing above it can judge the
//! device, and the player reports "playing" at a frozen position until someone
//! notices the silence. Every mature player bounds that wait (squeezelite at
//! 1 s, JACK at 1.5 periods, CamillaDSP at 2 chunks, the kernel's own blocking
//! write at ~1.1 rings) and treats running out of it as the device's fault.
//!
//! The other way a USB DAC dies — taking audio and throwing it away, so the
//! clock RACES — is the playback engine's `ClockWatch`; a write there returns
//! at once and never reaches this.
//!
//! # What counts as progress
//!
//! The queued amount (`snd_pcm_delay`) changing, in either direction. A
//! clocking device moves it every period. Only a full ring whose delay has not
//! moved for [`WriteStall::deadline`] is a stall, so a slow writer, a device
//! that is merely full, and a ring being refilled are all progress.

use std::time::{Duration, Instant};

/// The shortest stall that counts, in the device's own time.
///
/// Comfortably past anything a healthy device does while full: a period is
/// tens of milliseconds, and the longest ring a Pi is told to use is a second.
const MIN_STALL: Duration = Duration::from_secs(2);

/// Tracks one write's wait for room.
#[derive(Debug)]
pub(crate) struct WriteStall {
    deadline: Duration,
    /// When the wait began, and the delay it began at.
    since: Option<(Instant, usize)>,
}

impl WriteStall {
    /// A guard for a device with this ring, clocking at `rate × clock_speed`.
    ///
    /// The deadline is two rings, never less than [`MIN_STALL`], both in the
    /// device's time — so the virtual DAC's faster clock gets a proportionally
    /// shorter wall-clock deadline, and a test of it does not wait two seconds.
    pub(crate) fn new(buffer_frames: usize, sample_rate: u32, clock_speed: f64) -> Self {
        let device_secs_per_sec = clock_speed.max(0.01);
        let rate = f64::from(sample_rate.max(1));
        let two_rings = 2.0 * buffer_frames as f64 / rate;
        let device_time = two_rings.max(MIN_STALL.as_secs_f64());
        Self {
            deadline: Duration::from_secs_f64(device_time / device_secs_per_sec),
            since: None,
        }
    }

    /// How long a full ring may sit without its delay moving.
    #[cfg(test)]
    pub(crate) fn deadline(&self) -> Duration {
        self.deadline
    }

    /// The device took audio, or room appeared: whatever wait was running is
    /// over.
    pub(crate) fn progressed(&mut self) {
        self.since = None;
    }

    /// The ring is full and the delay reads `delay`. `Some(waited)` once it has
    /// read the same for longer than the deadline.
    pub(crate) fn full(&mut self, now: Instant, delay: usize) -> Option<Duration> {
        match self.since {
            Some((since, at)) if at == delay => {
                let waited = now.saturating_duration_since(since);
                (waited >= self.deadline).then_some(waited)
            }
            _ => {
                self.since = Some((now, delay));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_deadline_is_two_rings_and_never_under_two_seconds() {
        assert_eq!(WriteStall::new(5_512, 44_100, 1.0).deadline(), ms(2_000));
        // A two-second ring: two of them.
        assert_eq!(WriteStall::new(192_000, 96_000, 1.0).deadline(), ms(4_000));
        // The virtual DAC at 4x: the same device time, a quarter of the wait.
        assert_eq!(WriteStall::new(5_512, 44_100, 4.0).deadline(), ms(500));
    }

    #[test]
    fn a_delay_that_does_not_move_past_the_deadline_is_a_stall() {
        let mut stall = WriteStall::new(5_512, 44_100, 1.0);
        let t0 = Instant::now();
        assert_eq!(stall.full(t0, 5_512), None);
        assert_eq!(stall.full(t0 + ms(1_999), 5_512), None);
        assert_eq!(stall.full(t0 + ms(2_000), 5_512), Some(ms(2_000)));
    }

    /// A clocking device moves the delay every period, so a full ring that is
    /// being drained and refilled never adds up to a stall.
    #[test]
    fn a_moving_delay_restarts_the_wait() {
        let mut stall = WriteStall::new(5_512, 44_100, 1.0);
        let t0 = Instant::now();
        for i in 0..100u64 {
            let delay = 5_512 - (i % 2) as usize * 1_378;
            assert_eq!(stall.full(t0 + ms(i * 200), delay), None, "step {i}");
        }
    }

    #[test]
    fn progress_restarts_the_wait() {
        let mut stall = WriteStall::new(5_512, 44_100, 1.0);
        let t0 = Instant::now();
        assert_eq!(stall.full(t0, 5_512), None);
        stall.progressed();
        assert_eq!(stall.full(t0 + ms(1_500), 5_512), None);
        assert_eq!(stall.full(t0 + ms(3_000), 5_512), None);
        assert!(stall.full(t0 + ms(3_500), 5_512).is_some());
    }
}
