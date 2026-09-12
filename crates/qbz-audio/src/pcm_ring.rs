//! The decoded-audio ring: the renderer's entire tolerance for a stall.
//!
//! # What this is for
//!
//! Before it existed, one thread ran the whole chain from socket to
//! `snd_pcm_writei`: it pulled samples through the FLAC decoder, and the
//! decoder pulled bytes out of the network buffer, blocking when they had not
//! arrived. The only elasticity in the system was the ALSA hardware ring —
//! 125 ms at CD rate on the defaults — so a WiFi hiccup, an SD-card seek or a
//! slow decode longer than that emptied the hardware buffer and the DAC
//! clicked.
//!
//! This module is the cushion that goes between. A decoder thread fills it and
//! is free to block for as long as it likes; a writer thread drains it and
//! never blocks on anything but the device itself. Every serious renderer is
//! built this way — squeezelite keeps about ten seconds of decoded CD audio in
//! its output buffer, MPD pipes decoded chunks between its decoder and output
//! threads.
//!
//! # Why `ringbuf` and not something hand-written
//!
//! The storage is [`ringbuf`]'s lock-free SPSC ring, which this crate already
//! depends on and already uses for exactly this job on the JACK path
//! (`jack_backend.rs`). A second, hand-rolled ring beside it would have meant
//! either new `unsafe` or a mutex on the audio thread, plus a new primitive to
//! get right. What is added here is only what `ringbuf` does not provide: the
//! absolute frame counters, the track-boundary markers that ride alongside the
//! samples, and a wake channel so neither side has to spin.
//!
//! # The wake channel, and why the writer never touches the mutex
//!
//! The writer thread is a real-time thread (see [`crate::rt`]), so it must not
//! block on a lock a lower-priority thread can hold. It never does:
//!
//! - Decoder waiting for space: [`RingLink::wait_for_space`] — a condvar wait
//!   with a timeout, so a missed wakeup costs latency, never correctness.
//! - Writer signalling that it consumed: [`RingLink::notify_space`] — a bare
//!   `Condvar::notify_one`, which takes no lock.
//! - Writer waiting for data: it sleeps briefly. That path only runs when the
//!   ring has run dry, which is already the failure it exists to prevent, so
//!   the few milliseconds cost nothing that is not already lost.
//! - Boundaries: guarded by a mutex, but the writer reads an atomic first and
//!   only takes the lock on the one chunk per track that actually crosses a
//!   marker.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// A marker in the stream of decoded samples.
///
/// Keyed on the producer's absolute frame counter, so the writer can act on it
/// at the moment the frames around it reach the device rather than at the
/// moment they were decoded — which, with a ring seconds deep, are very
/// different times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// The producer's `frames_produced` when this marker was recorded, i.e. the
    /// absolute frame index at which the new state begins.
    pub at_frame: u64,
    pub kind: BoundaryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// A new source starts here — a gapless hand-off from the queue.
    TrackStart {
        /// Where this source sits in its TRACK, in frames.
        ///
        /// Non-zero for a source that was seeked or resumed: a seek to 2:46
        /// rebuilds the engine around a source whose first decoded frame is
        /// already 166 seconds into the song. Without this the writer would
        /// report the position as frames-since-this-source-started, which is
        /// what the player's clock then becomes — and everything keyed on the
        /// absolute position breaks with it. The QConnect buffering latch, for
        /// one, only releases once the player's clock passes the offset it
        /// seeked to, so a position that restarts at zero leaves the controller
        /// spinning on a track that is audibly playing.
        offset_frames: u64,
    },
    /// The decoder has no more sources. Everything before this is the tail.
    EndOfStream,
}

/// Shared state between the decoder and the writer, alongside the sample ring.
///
/// The samples themselves live in `ringbuf`'s producer/consumer halves, which
/// are moved into their respective threads. This is everything the two sides
/// have to agree on.
#[derive(Debug)]
pub struct RingLink {
    /// Frames the decoder has pushed. Monotonic.
    frames_produced: AtomicU64,
    /// Frames the writer has handed to the device. Monotonic, never ahead of
    /// `frames_produced`.
    frames_handed: AtomicU64,
    /// `at_frame` of the earliest unconsumed boundary, or `u64::MAX` for none.
    ///
    /// Exists so the writer can decide whether a boundary is due with one
    /// relaxed load instead of taking `boundaries` on every chunk.
    next_boundary_at: AtomicU64,
    boundaries: Mutex<VecDeque<Boundary>>,
    /// Set at shutdown to wake both sides out of any wait.
    closed: AtomicBool,
    /// Wake channel for the decoder. The mutex guards nothing but the condvar's
    /// own requirement; every wait has a timeout, so a lost notification costs
    /// a little latency and never a hang.
    space_lock: Mutex<()>,
    space: Condvar,
}

impl Default for RingLink {
    fn default() -> Self {
        Self::new()
    }
}

impl RingLink {
    pub fn new() -> Self {
        Self {
            frames_produced: AtomicU64::new(0),
            frames_handed: AtomicU64::new(0),
            next_boundary_at: AtomicU64::new(u64::MAX),
            boundaries: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            space_lock: Mutex::new(()),
            space: Condvar::new(),
        }
    }

    /// Producer: record `frames` more pushed into the ring.
    pub fn add_produced(&self, frames: u64) {
        self.frames_produced.fetch_add(frames, Ordering::Release);
    }

    /// Consumer: record `frames` more handed to the device.
    pub fn add_handed(&self, frames: u64) {
        self.frames_handed.fetch_add(frames, Ordering::Release);
    }

    pub fn frames_produced(&self) -> u64 {
        self.frames_produced.load(Ordering::Acquire)
    }

    pub fn frames_handed(&self) -> u64 {
        self.frames_handed.load(Ordering::Acquire)
    }

    /// Producer: mark that a new state begins at the current produced count.
    ///
    /// Called *before* the frames that follow it are pushed, so `at_frame` is
    /// the index of the first frame belonging to the new state.
    pub fn push_boundary(&self, kind: BoundaryKind) {
        let at_frame = self.frames_produced();
        let mut queue = match self.boundaries.lock() {
            Ok(q) => q,
            Err(poisoned) => poisoned.into_inner(),
        };
        queue.push_back(Boundary { at_frame, kind });
        // Only the FRONT matters to the writer's fast path.
        if let Some(front) = queue.front() {
            self.next_boundary_at
                .store(front.at_frame, Ordering::Release);
        }
    }

    /// Consumer: the next boundary, if the writer has now handed past it.
    ///
    /// Cheap when there is nothing to do — one relaxed load and a comparison —
    /// because it runs on the real-time thread once per chunk, while a boundary
    /// arrives once per track.
    pub fn take_due_boundary(&self) -> Option<Boundary> {
        let handed = self.frames_handed();
        if handed < self.next_boundary_at.load(Ordering::Acquire) {
            return None;
        }
        let mut queue = match self.boundaries.lock() {
            Ok(q) => q,
            Err(poisoned) => poisoned.into_inner(),
        };
        let due = match queue.front() {
            Some(front) if front.at_frame <= handed => queue.pop_front(),
            _ => None,
        };
        self.next_boundary_at.store(
            queue.front().map_or(u64::MAX, |b| b.at_frame),
            Ordering::Release,
        );
        due
    }

    /// Whether any boundary is still queued behind the one just taken.
    ///
    /// The writer needs this to tell a real end of stream from a stale one. The
    /// decoder marks `EndOfStream` when it runs out of sources, but the ring is
    /// seconds deep, so the writer does not reach that marker until seconds
    /// later — by which time a late gapless hand-off may already have queued a
    /// `TrackStart` behind it. Acting on the stale marker would drain the
    /// device and stop playback with the next track's audio already in the ring.
    pub fn has_queued_boundaries(&self) -> bool {
        match self.boundaries.lock() {
            Ok(q) => !q.is_empty(),
            Err(poisoned) => !poisoned.into_inner().is_empty(),
        }
    }

    /// Drop every pending boundary. For a teardown, where the queue's contents
    /// describe a stream nobody is going to play.
    pub fn clear_boundaries(&self) {
        let mut queue = match self.boundaries.lock() {
            Ok(q) => q,
            Err(poisoned) => poisoned.into_inner(),
        };
        queue.clear();
        self.next_boundary_at.store(u64::MAX, Ordering::Release);
    }

    /// Consumer: wake a decoder that is waiting for room.
    ///
    /// Takes no lock, deliberately: this is called from the real-time writer
    /// thread, which must never block on a mutex a normal-priority thread can
    /// be holding. The cost of the race that permits — the decoder checking the
    /// ring, finding it full, and missing this notification — is one wait
    /// timeout, which is why [`Self::wait_for_space`] always has one.
    pub fn notify_space(&self) {
        self.space.notify_one();
    }

    /// Producer: block until the writer signals it took something, `timeout`
    /// elapses, or the link is closed. Returns `false` if the link closed.
    pub fn wait_for_space(&self, timeout: Duration) -> bool {
        if self.is_closed() {
            return false;
        }
        let guard = match self.space_lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _unused = self.space.wait_timeout(guard, timeout);
        !self.is_closed()
    }

    /// Shut the link down and wake anything waiting on it.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.space.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Hard ceiling on the ring, in bytes.
///
/// The depth is expressed in seconds, and seconds cost very different amounts
/// at different rates: six seconds is 2.1 MB at 44.1 kHz stereo and 9.2 MB at
/// 192 kHz. This is the backstop against a configuration — or a future format —
/// where that arithmetic runs away, on a board where an unexpected 50 MB
/// allocation is the OOM killer.
const RING_MAX_BYTES: usize = 24 * 1024 * 1024;

/// Ring capacity in FRAMES for a stream, given the depth asked for.
///
/// `requested_ms` of `0` means "use `profile_seconds`", which is the host's
/// memory profile talking (see `MemoryProfile::pcm_ring_seconds`).
///
/// Two floors apply, and the second one is not optional:
///
/// - 250 ms, because a ring thinner than that is not a cushion.
/// - **Three times the ALSA ring.** The writer hands frames to the device and
///   only learns they were played later; while they are in flight the decoder
///   must still have somewhere to put the next ones. A decoded ring no deeper
///   than the hardware ring it feeds would spend its life full, throttling the
///   decoder against a hardware buffer it cannot drain any faster.
pub fn ring_capacity_frames(
    sample_rate: u32,
    channels: u16,
    alsa_buffer_frames: usize,
    requested_ms: u32,
    profile_seconds: u8,
) -> usize {
    let channels = usize::from(channels).max(1);
    let rate = u64::from(sample_rate.max(1));

    let ms = if requested_ms > 0 {
        u64::from(requested_ms)
    } else {
        u64::from(profile_seconds) * 1000
    };
    let from_ms = ((rate * ms) / 1000) as usize;

    let floor_ms = ((rate * 250) / 1000) as usize;
    let floor_alsa = alsa_buffer_frames.saturating_mul(3);
    let frames = from_ms.max(floor_ms).max(floor_alsa).max(1);

    // Trim to the byte ceiling rather than refusing: a smaller ring still
    // plays, and the alternative on a host that lands here is no audio at all.
    let max_frames = RING_MAX_BYTES / (channels * std::mem::size_of::<f32>());
    frames.min(max_frames.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_depth_asked_for_is_what_you_get_when_it_is_reasonable() {
        // 4 s at 44.1 kHz stereo.
        let frames = ring_capacity_frames(44_100, 2, 5_512, 4_000, 6);
        assert_eq!(frames, 176_400);
    }

    #[test]
    fn auto_takes_the_hosts_profile() {
        // 0 ms = auto; a Normal-class host says 6 s.
        let frames = ring_capacity_frames(44_100, 2, 5_512, 0, 6);
        assert_eq!(frames, 44_100 * 6);
        // A LowMemory host says 2 s, and gets exactly that.
        let frames = ring_capacity_frames(44_100, 2, 5_512, 0, 2);
        assert_eq!(frames, 44_100 * 2);
    }

    /// The floor that is not about taste: a decoded ring no deeper than the
    /// hardware ring it feeds spends its life full, and the decoder is
    /// throttled against a buffer it cannot make drain faster.
    #[test]
    fn the_ring_is_always_several_times_the_hardware_buffer() {
        // A host asking for a thin ring against a deliberately deep ALSA buffer
        // (audio.alsa_buffer_ms = 1000 is the Pi recommendation).
        let alsa = 44_100; // 1 s
        let frames = ring_capacity_frames(44_100, 2, alsa, 300, 6);
        assert!(
            frames >= alsa * 3,
            "ring {frames} frames is not 3x the {alsa}-frame hardware buffer"
        );
    }

    #[test]
    fn a_silly_small_request_is_raised_to_the_floor() {
        // Below the 250 ms floor, with a small hardware buffer so that floor is
        // the one that binds.
        let frames = ring_capacity_frames(44_100, 2, 512, 250, 6);
        assert!(frames >= 11_025, "got {frames}");
    }

    /// Seconds cost four times as much at 192 kHz as at 48 kHz, so the depth in
    /// seconds has to be bounded in bytes somewhere.
    #[test]
    fn the_byte_ceiling_binds_before_a_board_runs_out_of_memory() {
        let frames = ring_capacity_frames(192_000, 2, 96_000, 30_000, 6);
        let bytes = frames * 2 * std::mem::size_of::<f32>();
        assert!(bytes <= RING_MAX_BYTES, "{bytes} bytes");
        // And it is still a usable ring, not a token one.
        assert!(frames >= 192_000, "trimmed below one second: {frames}");
    }

    #[test]
    fn boundaries_come_due_in_order_and_only_once_handed_past() {
        let link = RingLink::new();
        link.add_produced(1000);
        link.push_boundary(BoundaryKind::TrackStart { offset_frames: 0 });
        link.add_produced(500);
        link.push_boundary(BoundaryKind::EndOfStream);

        // Nothing handed yet: nothing is due, and the fast path says so without
        // touching the queue.
        assert_eq!(link.take_due_boundary(), None);

        // Handed up to just before the first marker.
        link.add_handed(999);
        assert_eq!(link.take_due_boundary(), None);

        link.add_handed(1);
        assert_eq!(
            link.take_due_boundary(),
            Some(Boundary {
                at_frame: 1000,
                kind: BoundaryKind::TrackStart { offset_frames: 0 }
            })
        );
        // The second is not due yet even though the first was taken.
        assert_eq!(link.take_due_boundary(), None);

        link.add_handed(500);
        assert_eq!(
            link.take_due_boundary(),
            Some(Boundary {
                at_frame: 1500,
                kind: BoundaryKind::EndOfStream
            })
        );
        assert_eq!(link.take_due_boundary(), None);
    }

    /// The stale-end-of-stream case, which is the one that costs a track.
    #[test]
    fn a_successor_queued_behind_an_end_marker_is_visible_when_it_comes_due() {
        let link = RingLink::new();
        link.add_produced(1000);
        // The decoder ran out and said so...
        link.push_boundary(BoundaryKind::EndOfStream);
        // ...and then a late hand-off arrived, before the writer — seconds
        // behind, because the ring is seconds deep — ever reached the marker.
        link.push_boundary(BoundaryKind::TrackStart { offset_frames: 0 });

        link.add_handed(1000);
        let first = link.take_due_boundary().expect("end marker is due");
        assert_eq!(first.kind, BoundaryKind::EndOfStream);
        assert!(
            link.has_queued_boundaries(),
            "the successor must still be visible, or the writer stops on a stale marker"
        );
        assert_eq!(
            link.take_due_boundary().map(|b| b.kind),
            Some(BoundaryKind::TrackStart { offset_frames: 0 })
        );
        assert!(!link.has_queued_boundaries());
    }

    #[test]
    fn clearing_boundaries_also_clears_the_fast_path_hint() {
        let link = RingLink::new();
        link.push_boundary(BoundaryKind::TrackStart { offset_frames: 0 });
        link.clear_boundaries();
        link.add_handed(10_000);
        assert_eq!(link.take_due_boundary(), None);
    }

    /// A closed link must not leave the decoder parked: shutdown joins it.
    #[test]
    fn a_closed_link_stops_waiting_immediately() {
        let link = std::sync::Arc::new(RingLink::new());
        link.close();
        let started = std::time::Instant::now();
        assert!(!link.wait_for_space(Duration::from_secs(30)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "wait_for_space did not honour the close"
        );
    }

    #[test]
    fn a_waiting_producer_is_woken_by_the_consumer() {
        let link = std::sync::Arc::new(RingLink::new());
        let waiter = std::sync::Arc::clone(&link);
        let handle = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            waiter.wait_for_space(Duration::from_secs(30));
            started.elapsed()
        });
        // Give the waiter time to park, then wake it.
        std::thread::sleep(Duration::from_millis(50));
        link.notify_space();
        let waited = handle.join().expect("waiter panicked");
        assert!(
            waited < Duration::from_secs(5),
            "producer slept for {waited:?} despite being notified"
        );
    }
}
