//! A software DAC: an [`AudioOut`] that behaves like a sound card without being
//! one.
//!
//! # What it is for
//!
//! The engine's hardest behaviour to get right is not arithmetic, it is timing
//! against a device that consumes audio at its own pace: what a seek does to the
//! reported position, what a pause does to frames already committed, whether a
//! gapless hand-off is seamless, whether a short tail plays at all. None of that
//! can be tested against a real DAC in CI, and none of it can be tested without
//! a device that CLOCKS — which is why ALSA's own `null` PCM is no use here: it
//! accepts three seconds of audio in two milliseconds and reports no delay, so
//! every timing-dependent path degenerates.
//!
//! This clocks. It accepts frames at `sample_rate × speed` and reports a real
//! `delay_frames`, so `frames_handed - delay` — the identity the whole position
//! accounting rests on — means the same thing it does on hardware.
//!
//! # Speed
//!
//! `speed` multiplies the rate. At `1.0` a three-minute track takes three
//! minutes; at `200.0` it takes under a second, which is what makes a test of
//! a whole-track behaviour practical. Everything else is unchanged — the ring
//! still fills, the delay still means what it means, an underrun still happens
//! if the writer falls behind.
//!
//! # It models the parts that bite
//!
//! - **`start_threshold`.** Like the ALSA path, the clock does not start until
//!   a full ring has been written or someone calls
//!   [`AudioOut::start_if_prepared`]. A test that forgets the latter sees
//!   exactly what a Pi would: silence, no error.
//! - **Underruns.** If the ring empties while running, that is counted
//!   ([`VirtualAudioOut::underruns`]) instead of being silently absorbed.
//! - **Discard.** [`AudioOut::discard_queued`] throws away what is queued and
//!   returns to the un-started state, as `snd_pcm_drop` + `prepare` does.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::audio_out::AudioOut;

/// What the device is doing, mirroring the ALSA states that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockState {
    /// Configured, holding audio perhaps, but not consuming it yet.
    Prepared,
    /// Consuming at the nominal rate.
    Running,
}

#[derive(Debug)]
struct Clock {
    state: ClockState,
    /// Frames the device has consumed.
    played: u64,
    /// When `played` was last brought up to date.
    last_tick: Instant,
    /// Fractional frames carried between ticks, so a slow poll does not lose
    /// time to truncation.
    carry: f64,
}

/// An [`AudioOut`] backed by a virtual clock instead of a sound card.
#[derive(Debug)]
pub struct VirtualAudioOut {
    sample_rate: u32,
    channels: u16,
    buffer_frames: usize,
    period_frames: usize,
    speed: f64,
    clock: Mutex<Clock>,
    /// Frames handed over, ever. Monotonic across a discard, so a test can tell
    /// "played twice" from "played once".
    written: AtomicU64,
    /// Frames of SILENCE within `written`, so a test can tell padding from
    /// audio.
    silence_written: AtomicU64,
    /// Times the ring emptied while the clock was running.
    underruns: AtomicU64,
    /// Every frame handed over, in order, when recording is on.
    tape: Mutex<Option<Vec<f32>>>,
}

impl VirtualAudioOut {
    /// A device with the geometry a Pi would give you: a ring of `buffer_ms`
    /// and four periods in it.
    pub fn new(sample_rate: u32, channels: u16, buffer_ms: u32, speed: f64) -> Self {
        let buffer_frames =
            ((u64::from(sample_rate) * u64::from(buffer_ms)) / 1000).max(4) as usize;
        Self {
            sample_rate,
            channels,
            buffer_frames,
            period_frames: (buffer_frames / 4).max(1),
            speed: speed.max(0.01),
            clock: Mutex::new(Clock {
                state: ClockState::Prepared,
                played: 0,
                last_tick: Instant::now(),
                carry: 0.0,
            }),
            written: AtomicU64::new(0),
            silence_written: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            tape: Mutex::new(None),
        }
    }

    /// Keep every frame handed over, so a test can assert on the audio itself —
    /// that a gapless join has no silence in it, that a stop ramps down.
    ///
    /// Off by default: a whole track at 192 kHz is a lot of memory to keep for
    /// a test that only wants the position.
    pub fn record(&self) {
        *self.tape.lock().expect("tape") = Some(Vec::new());
    }

    /// Everything handed over since [`Self::record`], interleaved.
    pub fn recorded(&self) -> Vec<f32> {
        self.tape.lock().expect("tape").clone().unwrap_or_default()
    }

    /// Frames handed to the device, ever.
    pub fn frames_written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// Of those, how many were silence (keep-alive or the stop pad).
    pub fn silence_frames_written(&self) -> u64 {
        self.silence_written.load(Ordering::Acquire)
    }

    /// Frames the device has actually consumed.
    pub fn frames_played(&self) -> u64 {
        let mut clock = self.clock.lock().expect("clock");
        self.advance(&mut clock);
        clock.played
    }

    /// Times the ring ran dry while the clock was running — the virtual
    /// equivalent of an xrun.
    ///
    /// Sample this MID-TRACK. The end of a stream necessarily runs the ring dry
    /// and is counted here like any other, exactly as real ALSA reaches `XRun`
    /// at the end of a tail — `AlsaDirectStream::drain` relies on that to know
    /// the tail has finished. An end-of-track underrun is normal; one while
    /// audio is still flowing is the click this is for.
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Acquire)
    }

    /// `(played, queued, written)` sampled together under one lock.
    ///
    /// Reading `frames_played()` and `delay_frames()` separately is a race: the
    /// clock advances between the two calls, so `played + delay` need not equal
    /// `written` even though the invariant holds at every instant. Anything
    /// asserting on the relationship has to sample it atomically.
    pub fn snapshot(&self) -> (u64, usize, u64) {
        let mut clock = self.clock.lock().expect("clock");
        self.advance(&mut clock);
        let written = self.written.load(Ordering::Acquire);
        let queued = if clock.state == ClockState::Running {
            self.queued(&clock)
        } else {
            0
        };
        (clock.played, queued, written)
    }

    /// True once the clock is consuming.
    pub fn is_running(&self) -> bool {
        self.clock.lock().expect("clock").state == ClockState::Running
    }

    /// Bring `played` up to the present. Caller holds the lock.
    fn advance(&self, clock: &mut Clock) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(clock.last_tick);
        clock.last_tick = now;
        if clock.state != ClockState::Running {
            return;
        }
        let exact = elapsed.as_secs_f64() * f64::from(self.sample_rate) * self.speed + clock.carry;
        let whole = exact.floor();
        clock.carry = exact - whole;
        let written = self.written.load(Ordering::Acquire);
        let wanted = clock.played.saturating_add(whole as u64);
        if wanted > written {
            // The ring ran dry with the clock running. On hardware this is an
            // xrun; here it is counted so a test can assert it did not happen.
            self.underruns.fetch_add(1, Ordering::Relaxed);
            clock.played = written;
        } else {
            clock.played = wanted;
        }
    }

    /// Queued frames, and whether the clock should now auto-start. Caller holds
    /// the lock.
    fn queued(&self, clock: &Clock) -> usize {
        self.written
            .load(Ordering::Acquire)
            .saturating_sub(clock.played) as usize
    }

    /// Accept up to `frames`, blocking (in virtual time) until there is room.
    fn accept(&self, frames: usize, cancel: &AtomicBool, silence: bool) -> usize {
        let mut accepted = 0usize;
        while accepted < frames {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let room = {
                let mut clock = self.clock.lock().expect("clock");
                self.advance(&mut clock);
                let room = self.buffer_frames.saturating_sub(self.queued(&clock));
                if room == 0 {
                    // Full. On hardware the writer waits on the device; here it
                    // waits for the virtual clock to consume a period. Bounded,
                    // so a cancel is always noticed.
                    drop(clock);
                    std::thread::sleep(self.period_wait());
                    continue;
                }
                room
            };
            let take = room.min(frames - accepted);
            self.written.fetch_add(take as u64, Ordering::AcqRel);
            if silence {
                self.silence_written
                    .fetch_add(take as u64, Ordering::AcqRel);
            }
            accepted += take;

            // `start_threshold`: the ALSA path does not clock until a whole
            // ring has been written, and neither does this. A test that never
            // fills the ring and never calls `start_if_prepared` gets silence
            // and no error — exactly what the hardware does.
            let mut clock = self.clock.lock().expect("clock");
            if clock.state == ClockState::Prepared && self.queued(&clock) >= self.buffer_frames {
                clock.state = ClockState::Running;
                clock.last_tick = Instant::now();
                clock.carry = 0.0;
            }
        }
        accepted
    }

    /// How long to wait for the clock to free a period, in real time.
    fn period_wait(&self) -> Duration {
        let secs = self.period_frames as f64 / (f64::from(self.sample_rate) * self.speed);
        // Floored so a very fast virtual clock does not spin the CPU, capped so
        // a slow one still notices a cancel.
        Duration::from_secs_f64(secs.clamp(0.0005, 0.050))
    }
}

impl AudioOut for VirtualAudioOut {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn buffer_frames(&self) -> usize {
        self.buffer_frames
    }

    fn period_frames(&self) -> usize {
        self.period_frames
    }

    fn write_f32(&self, samples: &[f32], cancel: &AtomicBool) -> Result<usize, String> {
        let channels = usize::from(self.channels).max(1);
        let frames = samples.len() / channels;
        let accepted = self.accept(frames, cancel, false);
        if let Some(tape) = self.tape.lock().expect("tape").as_mut() {
            tape.extend_from_slice(&samples[..accepted * channels]);
        }
        Ok(accepted)
    }

    fn write_silence_to_depth(&self, target_ms: u32, cancel: &AtomicBool) -> Result<usize, String> {
        if target_ms == 0 {
            return Ok(0);
        }
        let target = ((u64::from(self.sample_rate) * u64::from(target_ms)) / 1000) as usize;
        let target = target.min(self.buffer_frames);
        let held = {
            let mut clock = self.clock.lock().expect("clock");
            self.advance(&mut clock);
            self.queued(&clock)
        };
        let needed = target.saturating_sub(held);
        if needed == 0 {
            return Ok(0);
        }
        let accepted = self.accept(needed, cancel, true);
        if let Some(tape) = self.tape.lock().expect("tape").as_mut() {
            tape.extend(std::iter::repeat_n(
                0.0,
                accepted * usize::from(self.channels).max(1),
            ));
        }
        // Like the real one: a depth that is a fraction of the ring can never
        // reach the start threshold on its own, so say so explicitly.
        self.start_if_prepared()?;
        Ok(accepted)
    }

    fn delay_frames(&self) -> usize {
        let mut clock = self.clock.lock().expect("clock");
        self.advance(&mut clock);
        if clock.state != ClockState::Running {
            return 0;
        }
        self.queued(&clock)
    }

    fn start_if_prepared(&self) -> Result<(), String> {
        let mut clock = self.clock.lock().expect("clock");
        if clock.state == ClockState::Prepared {
            clock.state = ClockState::Running;
            clock.last_tick = Instant::now();
            clock.carry = 0.0;
        }
        Ok(())
    }

    fn discard_queued(&self) -> Result<(), String> {
        let mut clock = self.clock.lock().expect("clock");
        self.advance(&mut clock);
        // Everything queued is thrown away: the device has "played" it in the
        // sense that it will never come back, but nothing was heard.
        clock.played = self.written.load(Ordering::Acquire);
        clock.state = ClockState::Prepared;
        clock.carry = 0.0;
        Ok(())
    }

    fn drain(&self) -> Result<(), String> {
        // Let the tail out, then stop. Bounded like the real drain, so a device
        // that never consumes cannot hang a test.
        self.start_if_prepared()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let mut clock = self.clock.lock().expect("clock");
                self.advance(&mut clock);
                if self.queued(&clock) == 0 {
                    clock.state = ClockState::Prepared;
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err("virtual drain deadline".to_string());
            }
            std::thread::sleep(self.period_wait());
        }
    }

    fn stop(&self) -> Result<(), String> {
        self.discard_queued()
    }

    fn set_hardware_volume(&self, _volume: f32) -> Result<(), String> {
        Err("the virtual device has no mixer".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// The property ALSA's own `null` device fails, and the reason this exists:
    /// audio must take time to play.
    #[test]
    fn the_clock_consumes_at_the_nominal_rate() {
        // 100 ms ring, 100x speed: one second of audio should take ~10 ms.
        let out = VirtualAudioOut::new(44_100, 2, 100, 100.0);
        let frames = 44_100usize;
        let samples = vec![0.25f32; frames * 2];

        let started = Instant::now();
        let mut written = 0;
        while written < frames {
            written += out.write_f32(&samples[written * 2..], &cancel()).unwrap();
        }
        out.drain().unwrap();
        let elapsed = started.elapsed();

        assert_eq!(out.frames_written(), frames as u64);
        assert!(
            elapsed >= Duration::from_millis(5),
            "a second of audio went through in {elapsed:?} — the clock is not clocking"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "took {elapsed:?}; the speed multiplier is not being applied"
        );
    }

    /// `start_threshold` modelled: a partial ring does not clock by itself.
    /// This is the trap that makes a short track silent on hardware.
    #[test]
    fn a_partial_ring_does_not_start_until_it_is_told_to() {
        let out = VirtualAudioOut::new(44_100, 2, 1_000, 100.0);
        // A tenth of the ring.
        let frames = out.buffer_frames() / 10;
        out.write_f32(&vec![0.1f32; frames * 2], &cancel()).unwrap();

        assert!(
            !out.is_running(),
            "a tenth of a ring must not start the clock"
        );
        assert_eq!(out.delay_frames(), 0, "a stopped device owes nothing");

        out.start_if_prepared().unwrap();
        assert!(out.is_running(), "an explicit start must start it");
        assert!(out.delay_frames() > 0, "now it owes the frames it holds");
    }

    #[test]
    fn a_full_ring_starts_the_clock_on_its_own() {
        let out = VirtualAudioOut::new(44_100, 2, 100, 100.0);
        let frames = out.buffer_frames();
        out.write_f32(&vec![0.1f32; frames * 2], &cancel()).unwrap();
        assert!(out.is_running(), "a full ring reaches the start threshold");
    }

    /// The identity the position accounting rests on.
    #[test]
    fn played_plus_delay_is_everything_written() {
        let out = VirtualAudioOut::new(44_100, 2, 100, 50.0);
        let frames = out.buffer_frames();
        out.write_f32(&vec![0.1f32; frames * 2], &cancel()).unwrap();
        std::thread::sleep(Duration::from_millis(1));
        // Sampled atomically: reading the two separately lets the clock advance
        // between them, which is a race, not a broken invariant.
        let (played, queued, written) = out.snapshot();
        assert_eq!(
            played + queued as u64,
            written,
            "played + queued must account for everything handed over"
        );
    }

    #[test]
    fn a_discard_throws_the_queue_away_and_stops_the_clock() {
        let out = VirtualAudioOut::new(44_100, 2, 100, 100.0);
        out.write_f32(&vec![0.1f32; out.buffer_frames() * 2], &cancel())
            .unwrap();
        assert!(out.is_running());
        out.discard_queued().unwrap();
        assert!(
            !out.is_running(),
            "a discard returns the device to prepared"
        );
        assert_eq!(out.delay_frames(), 0, "and it owes nothing");
    }

    #[test]
    fn a_cancelled_write_reports_what_it_actually_accepted() {
        let out = VirtualAudioOut::new(44_100, 2, 50, 1.0);
        let cancel = AtomicBool::new(true);
        // Offer far more than the ring can hold, already cancelled.
        let accepted = out
            .write_f32(&vec![0.1f32; out.buffer_frames() * 4], &cancel)
            .unwrap();
        assert!(
            accepted <= out.buffer_frames(),
            "accepted {accepted} frames into a {}-frame ring",
            out.buffer_frames()
        );
    }

    #[test]
    fn silence_tops_up_to_the_depth_and_is_counted_separately() {
        // REAL time (speed 1.0) deliberately. `write_silence_to_depth` starts
        // the clock, so under a 100x clock the microseconds between the two
        // calls below drain a visible number of frames and the second top-up
        // is no longer zero — which is the harness being right, not wrong.
        let out = VirtualAudioOut::new(44_100, 2, 1_000, 1.0);
        let written = out.write_silence_to_depth(100, &cancel()).unwrap();
        assert!(written > 0);
        assert_eq!(out.silence_frames_written(), written as u64);
        // A second top-up at the same depth writes at most the sliver that
        // drained in between — never another depth's worth.
        let again = out.write_silence_to_depth(100, &cancel()).unwrap();
        assert!(
            again * 10 < written,
            "a second top-up wrote {again} of {written} frames — the depth is a ceiling, \
             not a target to refill from scratch"
        );
    }

    #[test]
    fn the_tape_keeps_what_was_handed_over() {
        let out = VirtualAudioOut::new(44_100, 2, 100, 100.0);
        out.record();
        out.write_f32(&[0.5, -0.5, 0.25, -0.25], &cancel()).unwrap();
        assert_eq!(out.recorded(), vec![0.5, -0.5, 0.25, -0.25]);
    }
}
