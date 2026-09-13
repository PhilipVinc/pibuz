//! Playback Engine Abstraction
//!
//! Unified interface for different playback backends:
//! - Rodio (PipeWire, Pulse, ALSA via CPAL) - uses rodio::Sink
//! - ALSA Direct (hw: devices) - bypasses rodio, writes directly to ALSA PCM
//!
//! ALSA Direct uses a single long-lived writer thread with a source queue
//! to enable gapless playback. When one source ends, the next is picked up
//! seamlessly without interrupting the PCM stream.

use qbz_audio::pcm_ring::{BoundaryKind, RingLink};
use qbz_audio::AudioOut;
// The PCM engine takes `dyn AudioOut`; the Jack variant still names its
// concrete stream. `AlsaDirectStream` was imported here for the DoP variant
// only — with that gone, only the LINUX build can see the import is unused,
// which is the same cfg trap CLAUDE.md records, running the other way.
#[cfg(target_os = "linux")]
use qbz_audio::JackStream;
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rodio::{mixer::Mixer, Player as RodioPlayer, Source};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// A boxed sample iterator that can be sent across threads
type BoxedSampleIter = Box<dyn Iterator<Item = f32> + Send>;

/// A source queued for playback, with where it sits in its own track.
///
/// `start_frame` is zero for a track played from the beginning and non-zero for
/// one that was seeked or resumed. It has to travel WITH the source rather than
/// being a property of the engine, because a gapless hand-off queues the next
/// track (offset 0) while a seeked one (offset 166 s, say) is still playing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct QueuedSource {
    iter: BoxedSampleIter,
    start_frame: u64,
}

/// Process-wide decoded-ring depth in ms; `0` takes the host memory profile's
/// figure. Set once at player start from `audio.pcm_ring_ms`, mirroring how
/// `alsa_direct::set_alsa_buffer_ms` is wired.
static PCM_RING_MS: AtomicU32 = AtomicU32::new(0);

/// Set the decoded-ring depth in ms; `0` restores the profile default.
pub fn set_pcm_ring_ms(ms: u32) {
    PCM_RING_MS.store(ms, Ordering::Relaxed);
}

/// Configured decoded-ring depth in ms; `0` when it is the profile's to choose.
pub fn pcm_ring_ms() -> u32 {
    PCM_RING_MS.load(Ordering::Relaxed)
}

/// Thread-safe source queue for gapless playback.
/// The writer thread consumes sources; append() pushes new ones.
pub(crate) struct SourceQueue<S> {
    queue: Mutex<VecDeque<S>>,
    /// Notifies the writer thread that a new source is available
    notify: Condvar,
}

impl<S> SourceQueue<S> {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            notify: Condvar::new(),
        }
    }

    /// Push a new source to the back of the queue
    fn push(&self, source: S) {
        let mut q = self.queue.lock().unwrap();
        q.push_back(source);
        self.notify.notify_one();
    }

    /// Try to pop the next source (non-blocking)
    fn try_pop(&self) -> Option<S> {
        let mut q = self.queue.lock().unwrap();
        q.pop_front()
    }

    /// Wait for a source to become available (with timeout)
    /// Returns None on timeout (used to check stop/pause flags)
    fn wait_for_source(&self, timeout: Duration) -> Option<S> {
        let mut q = self.queue.lock().unwrap();
        if q.is_empty() {
            let (guard, _) = self.notify.wait_timeout(q, timeout).unwrap();
            q = guard;
        }
        q.pop_front()
    }

    fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}

/// Unified playback engine
pub enum PlaybackEngine {
    /// Rodio-based (PipeWire, Pulse, ALSA via CPAL)
    Rodio { sink: RodioPlayer },
    /// Direct ALSA (hw: devices, bit-perfect) with gapless source queue
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AlsaDirect {
        /// The output, behind a trait so the engine can be driven against a
        /// virtual device in a test. See `qbz_audio::audio_out`.
        stream: Arc<dyn AudioOut>,
        is_playing: Arc<AtomicBool>,
        should_stop: Arc<AtomicBool>,
        position_frames: Arc<AtomicU64>,
        source_queue: Arc<SourceQueue<QueuedSource>>,
        /// Decodes sources into the ring. Ordinary priority, and ALLOWED to
        /// block — on the network, on the disk, on symphonia. That is the whole
        /// point of it being a separate thread.
        decoder_thread: Option<thread::JoinHandle<()>>,
        /// Drains the ring into ALSA. Real-time (see `qbz_audio::rt`) and must
        /// never block on anything but the device. Nothing that can wait on I/O
        /// may ever be added to it.
        writer_thread: Option<thread::JoinHandle<()>>,
        /// Shared counters, boundaries and wake channel between the two.
        link: Arc<RingLink>,
        /// Signals that the writer thread has consumed a source and moved to next
        source_transition: Arc<AtomicBool>,
        hardware_volume: bool,
        /// Software volume for the writer thread, as f32 bits — the same
        /// idiom `SharedState::volume` and the normalization `gain_atomic`
        /// use. Only read when `hardware_volume` is false; unity means the
        /// writer passes samples through untouched.
        volume: Arc<AtomicU32>,
    },
    /// Native JACK output (#263 Tier 3). Mirrors AlsaDirect (gapless source queue
    /// + a single long-lived feeder thread), but the feeder resamples each source
    ///   to the JACK graph rate and writes interleaved stereo f32 into the client's
    ///   lock-free ring buffer via `JackStream::write_f32`. NOT bit-perfect.
    #[cfg(target_os = "linux")]
    Jack {
        is_playing: Arc<AtomicBool>,
        should_stop: Arc<AtomicBool>,
        position_frames: Arc<AtomicU64>,
        source_queue: Arc<SourceQueue<BoxedSampleIter>>,
        feeder_thread: Option<thread::JoinHandle<()>>,
        source_transition: Arc<AtomicBool>,
        graph_rate: u32,
    },
}

impl PlaybackEngine {
    /// Create Rodio engine
    pub fn new_rodio(mixer: &Mixer) -> Result<Self, String> {
        let sink = RodioPlayer::connect_new(mixer);
        Ok(Self::Rodio { sink })
    }

    /// Create ALSA Direct engine with gapless source queue.
    /// Spawns a single writer thread that lives for the engine's lifetime.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn new_alsa_direct(stream: Arc<dyn AudioOut>, hardware_volume: bool) -> Self {
        let is_playing = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::new(AtomicBool::new(false));
        let position_frames = Arc::new(AtomicU64::new(0));
        let source_queue = Arc::new(SourceQueue::new());
        let source_transition = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let link = Arc::new(RingLink::new());

        let channels = stream.channels();
        let capacity_frames = qbz_audio::pcm_ring::ring_capacity_frames(
            stream.sample_rate(),
            channels,
            stream.buffer_frames(),
            pcm_ring_ms(),
            qbz_models::system_capabilities::memory_profile().pcm_ring_seconds,
        );
        log::info!(
            "[ALSA Direct Engine] decoded ring: {capacity_frames} frames ({} ms at {} Hz, {} KB), \
             hardware ring {} frames, period {}",
            (capacity_frames as u64 * 1000) / u64::from(stream.sample_rate()).max(1),
            stream.sample_rate(),
            (capacity_frames * usize::from(channels) * 4) / 1024,
            stream.buffer_frames(),
            stream.period_frames(),
        );

        let (producer, consumer) =
            HeapRb::<f32>::new(capacity_frames * usize::from(channels).max(1)).split();

        // The DECODER holds no `Arc<AlsaDirectStream>`, and that is a
        // deliberate safety property rather than an accident of what it needs.
        //
        // The stream owns the exclusive `hw:` handle and the D-Bus device
        // reservation, and issue #521 is what happens when a surviving clone of
        // it outlives a stop: the device stays open and the NEXT stream fails to
        // start. The decoder is the thread that CAN get stuck — it is the one
        // allowed to block on the network — so if a stop ever has to abandon a
        // thread, it must be this one, and abandoning it must not strand the
        // device. Keeping the stream out of its captures is what guarantees
        // that. Do not hand it one.
        let decoder = {
            let stop_c = should_stop.clone();
            let queue_c = source_queue.clone();
            let link_c = link.clone();
            thread::spawn(move || {
                alsa_decoder_thread(producer, link_c, stop_c, queue_c, channels);
            })
        };

        let writer = {
            let stream_c = stream.clone();
            let playing_c = is_playing.clone();
            let stop_c = should_stop.clone();
            let pos_c = position_frames.clone();
            let link_c = link.clone();
            let transition_c = source_transition.clone();
            let volume_c = volume.clone();
            thread::spawn(move || {
                alsa_writer_thread(
                    consumer,
                    stream_c,
                    link_c,
                    playing_c,
                    stop_c,
                    pos_c,
                    transition_c,
                    volume_c,
                    channels,
                );
            })
        };

        Self::AlsaDirect {
            stream,
            is_playing,
            should_stop,
            position_frames,
            source_queue,
            decoder_thread: Some(decoder),
            writer_thread: Some(writer),
            link,
            source_transition,
            hardware_volume,
            volume,
        }
    }

    /// Create a JACK engine with a gapless source queue (#263 Tier 3). Spawns one
    /// long-lived feeder thread that resamples each source to the JACK graph rate
    /// and writes it to the client's ring buffer.
    #[cfg(target_os = "linux")]
    pub fn new_jack(stream: Arc<JackStream>) -> Self {
        let is_playing = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::new(AtomicBool::new(false));
        let position_frames = Arc::new(AtomicU64::new(0));
        let source_queue = Arc::new(SourceQueue::new());
        let source_transition = Arc::new(AtomicBool::new(false));
        let graph_rate = stream.sample_rate();

        let handle = {
            let stream_c = stream.clone();
            let playing_c = is_playing.clone();
            let stop_c = should_stop.clone();
            let pos_c = position_frames.clone();
            let queue_c = source_queue.clone();
            let transition_c = source_transition.clone();
            thread::spawn(move || {
                jack_feeder_thread(stream_c, playing_c, stop_c, pos_c, queue_c, transition_c);
            })
        };

        Self::Jack {
            is_playing,
            should_stop,
            position_frames,
            source_queue,
            feeder_thread: Some(handle),
            source_transition,
            graph_rate,
        }
    }

    /// Append an audio source, saying where in its own track it begins.
    ///
    /// `start_offset_secs` is 0 for a track played from the start, and the seek
    /// or resume offset otherwise. It travels with the source because it is a
    /// property of the source, not of the engine: a gapless hand-off queues the
    /// NEXT track at 0 while a seeked one is still playing.
    ///
    /// Getting it wrong is not cosmetic. The ALSA writer reports the position
    /// from the audio clock, and everything downstream treats that as the
    /// position within the TRACK — including the QConnect buffering latch,
    /// which only releases once the clock passes the offset that was seeked to.
    /// Report frames-since-this-source-started and a seek leaves the controller
    /// spinning, its clock frozen, on a track that is audibly playing.
    pub fn append<S>(&mut self, source: S, start_offset_secs: u64) -> Result<(), String>
    where
        S: Source<Item = f32> + Send + 'static,
    {
        match self {
            Self::Rodio { sink } => {
                sink.append(source);
                Ok(())
            }
            Self::AlsaDirect {
                stream,
                is_playing,
                should_stop,
                position_frames,
                source_queue,
                source_transition,
                ..
            } => {
                let is_first = source_queue.is_empty() && !is_playing.load(Ordering::SeqCst);

                // Seconds to frames here, where the stream's rate is at hand;
                // the decoder and writer deal only in frames.
                let start_frame = start_offset_secs.saturating_mul(u64::from(stream.sample_rate()));
                source_queue.push(QueuedSource {
                    iter: Box::new(source.into_iter()),
                    start_frame,
                });

                if is_first {
                    // First source: reset position, clear stop, start playing
                    position_frames.store(start_frame, Ordering::SeqCst);
                    should_stop.store(false, Ordering::SeqCst);
                    source_transition.store(false, Ordering::SeqCst);
                    is_playing.store(true, Ordering::SeqCst);
                    log::info!("[ALSA Direct Engine] First source queued, playback starting");
                } else {
                    log::info!("[ALSA Direct Engine] Source queued for gapless transition");
                }

                Ok(())
            }
            #[cfg(target_os = "linux")]
            Self::Jack {
                is_playing,
                should_stop,
                position_frames,
                source_queue,
                source_transition,
                graph_rate,
                ..
            } => {
                let is_first = source_queue.is_empty() && !is_playing.load(Ordering::SeqCst);
                // JACK does not report a position to a controller, so the track
                // offset is not plumbed through it.
                let _ = start_offset_secs;
                // Resample the track-native source to the JACK graph rate (stereo) so
                // the feeder/ring always carry graph-rate interleaved stereo f32.
                let resampled = rodio::source::UniformSourceIterator::new(
                    source,
                    std::num::NonZero::new(2u16).unwrap(),
                    std::num::NonZero::new(*graph_rate).unwrap(),
                );
                let boxed: BoxedSampleIter = Box::new(resampled);
                source_queue.push(boxed);
                if is_first {
                    position_frames.store(0, Ordering::SeqCst);
                    should_stop.store(false, Ordering::SeqCst);
                    source_transition.store(false, Ordering::SeqCst);
                    is_playing.store(true, Ordering::SeqCst);
                    log::info!("[JACK Engine] First source queued, playback starting");
                } else {
                    log::info!("[JACK Engine] Source queued for gapless transition");
                }
                Ok(())
            }
        }
    }

    /// Play (unpause)
    pub fn play(&self) {
        match self {
            Self::Rodio { sink } => sink.play(),
            Self::AlsaDirect { is_playing, .. } => {
                log::info!("[ALSA Direct Engine] Resume requested");
                is_playing.store(true, Ordering::SeqCst);
            }
            #[cfg(target_os = "linux")]
            Self::Jack { is_playing, .. } => {
                log::info!("[JACK Engine] Resume requested");
                is_playing.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Pause
    pub fn pause(&self) {
        match self {
            Self::Rodio { sink } => sink.pause(),
            Self::AlsaDirect { is_playing, .. } => {
                log::info!("[ALSA Direct Engine] Pause requested");
                is_playing.store(false, Ordering::SeqCst);
            }
            #[cfg(target_os = "linux")]
            Self::Jack { is_playing, .. } => {
                log::info!("[JACK Engine] Pause requested");
                is_playing.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Stop playback and release resources.
    /// For ALSA Direct, signals the writer thread and waits for it to exit.
    /// The Drop impl handles the same cleanup if stop() is not called explicitly.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    /// Internal stop logic shared by stop() and Drop
    fn stop_inner(&mut self) {
        match self {
            Self::Rodio { sink } => {
                sink.stop();
            }
            Self::AlsaDirect {
                stream,
                is_playing,
                should_stop,
                decoder_thread,
                writer_thread,
                link,
                ..
            } => {
                if should_stop.load(Ordering::SeqCst) {
                    return; // Already stopped
                }
                log::info!("[ALSA Direct Engine] Stop requested");
                should_stop.store(true, Ordering::SeqCst);
                is_playing.store(false, Ordering::SeqCst);
                // Wake anything parked on the ring so both threads get to see
                // `should_stop` immediately rather than at the next timeout.
                link.close();

                // ONE deadline for BOTH threads, not one each.
                //
                // Stop is called inline from the audio command thread, which is
                // the same thread that serves play, pause and seek. Two serial
                // three-second graces would make a single stop block that
                // thread for six, and the user would feel it as a renderer that
                // ignores the transport for six seconds.
                //
                // Bounded at all for the original reason: a thread that will
                // not exit must not be joined. An ALSA write against a device
                // that has stopped draining used to pin the writer forever, and
                // an unbounded join turned that into a permanent, silent loss
                // of the audio thread — no playback for the life of the
                // process, with the position counter still ticking. Seen once
                // on hardware.
                const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);
                let deadline = std::time::Instant::now() + EXIT_GRACE;
                let mut abandoned = Vec::new();
                for (name, handle) in [
                    ("writer", writer_thread.take()),
                    ("decoder", decoder_thread.take()),
                ] {
                    let Some(handle) = handle else { continue };
                    while !handle.is_finished() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    if handle.is_finished() {
                        let _ = handle.join();
                    } else {
                        abandoned.push(name);
                    }
                }
                if !abandoned.is_empty() {
                    // The DECODER is the thread that can legitimately be stuck
                    // — it is the one allowed to block on the network — and it
                    // holds no `AlsaDirectStream`, so abandoning it strands
                    // nothing: the device closes below and the next stream
                    // opens cleanly. That is the #521 hazard, and the split is
                    // what defuses it. A stuck WRITER is the serious one.
                    log::error!(
                        "[ALSA Direct Engine] {} thread(s) did not exit within {:?}: {}. \
                         Abandoning the join so the audio thread survives.",
                        abandoned.len(),
                        EXIT_GRACE,
                        abandoned.join(", ")
                    );
                }

                if let Err(e) = stream.stop() {
                    log::warn!("[ALSA Direct Engine] Stop failed: {}", e);
                }
            }
            #[cfg(target_os = "linux")]
            Self::Jack {
                is_playing,
                should_stop,
                feeder_thread,
                ..
            } => {
                if should_stop.load(Ordering::SeqCst) {
                    return;
                }
                log::info!("[JACK Engine] Stop requested");
                should_stop.store(true, Ordering::SeqCst);
                is_playing.store(false, Ordering::SeqCst);
                if let Some(handle) = feeder_thread.take() {
                    let _ = handle.join();
                }
                // JackStream's Drop deactivates the client + unregisters the ports.
            }
        }
    }

    /// Set volume (0.0 - 1.0)
    ///
    /// The fraction is the slider's position, not the amplitude multiplier:
    /// software paths bend it through the configured curve
    /// (`qbz_audio::volume_curve`), because scaling samples by the fraction
    /// itself puts every useful listening level in the bottom tenth of the
    /// travel. The hardware-mixer path passes it through untouched — an ALSA
    /// mixer applies the card's own dB mapping.
    pub fn set_volume(&self, volume: f32) {
        let software_gain = qbz_audio::volume_curve::gain_for(volume);
        match self {
            Self::Rodio { sink } => sink.set_volume(software_gain),
            Self::AlsaDirect {
                stream,
                hardware_volume,
                volume: software_volume,
                ..
            } => {
                // `stream` is only read by the Linux hardware-volume path below.
                // Do NOT let a non-Linux build talk you into binding it as `_`:
                // that compiles here and breaks the ALSA mixer on the target.
                #[cfg(not(target_os = "linux"))]
                let _ = stream;
                if *hardware_volume {
                    #[cfg(target_os = "linux")]
                    {
                        if let Err(e) = stream.set_hardware_volume(volume) {
                            log::warn!("[ALSA Direct Engine] Hardware volume failed: {}", e);
                        }
                    }
                } else {
                    // Hand it to the writer thread to apply. This branch used to
                    // do nothing at all, which left a DAC with no mixer element
                    // (a fixed-output design like the Schiit Modius) with no
                    // volume control whatsoever the moment playback went direct
                    // — the controlling app's slider moved and the level did
                    // not. Unity is still bit-perfect: see alsa_writer_thread.
                    software_volume.store(software_gain.to_bits(), Ordering::Relaxed);
                }
            }
            #[cfg(target_os = "linux")]
            Self::Jack { .. } => {
                // JACK output volume is controlled in the JACK graph / DAW; the
                // feeder writes unattenuated f32. (Software volume could later be
                // applied by scaling in the feeder.)
            }
        }
    }

    /// Check if playback queue is empty (all sources consumed, not playing)
    pub fn empty(&self) -> bool {
        match self {
            Self::Rodio { sink } => sink.empty(),
            Self::AlsaDirect {
                is_playing,
                source_queue,
                ..
            } => !is_playing.load(Ordering::SeqCst) && source_queue.is_empty(),
            #[cfg(target_os = "linux")]
            Self::Jack {
                is_playing,
                source_queue,
                ..
            } => !is_playing.load(Ordering::SeqCst) && source_queue.is_empty(),
        }
    }

    /// Check if a gapless source transition just happened.
    /// Returns true once, then resets the flag.
    pub fn take_source_transition(&self) -> bool {
        match self {
            Self::Rodio { .. } => false,
            Self::AlsaDirect {
                source_transition, ..
            } => source_transition
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            #[cfg(target_os = "linux")]
            Self::Jack {
                source_transition, ..
            } => source_transition
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
        }
    }

    /// Position within the current track, in seconds, from the AUDIO clock.
    ///
    /// `None` for the backends that do not track it. On ALSA Direct this is
    /// derived from frames the DAC has actually converted — frames handed over
    /// minus `snd_pcm_delay` — so it is what the listener is hearing, not what
    /// the writer has written. The two differ by the whole hardware ring, which
    /// is up to a second on the buffer a Pi is told to use.
    /// Lowest decoded-ring fill since this was last asked, in frames, with the
    /// number of reads that found it empty.
    ///
    /// Drains the figure, so the caller sees the worst case in ITS interval
    /// rather than the worst since the stream opened. `None` on a backend
    /// without the ring, or before any read.
    ///
    /// Exists because an audible artifact was reported with nothing at all in
    /// the log: no xrun, no PCM recovery, no error on any path. A stall the
    /// ring absorbs leaves no trace, and one it only just absorbs leaves the
    /// same trace as one it did not. This is the number that distinguishes
    /// them, and it has to be read from a thread that is allowed to log —
    /// never from the writer, which is real-time.
    pub fn take_ring_low_water(&self) -> Option<(u64, u64)> {
        match self {
            #[cfg(target_os = "linux")]
            Self::AlsaDirect { link, .. } => link.take_low_water(),
            _ => None,
        }
    }

    pub fn position_secs(&self) -> Option<u64> {
        match self {
            Self::Rodio { .. } => None,
            Self::AlsaDirect {
                position_frames,
                stream,
                ..
            } => {
                let frames = position_frames.load(Ordering::SeqCst);
                let sample_rate = stream.sample_rate() as u64;
                Some(frames / sample_rate)
            }
            #[cfg(target_os = "linux")]
            Self::Jack {
                position_frames,
                graph_rate,
                ..
            } => {
                let frames = position_frames.load(Ordering::SeqCst);
                Some(frames / (*graph_rate as u64).max(1))
            }
        }
    }
}

/// How long an idle turn of the writer thread may wait before topping the
/// keep-alive silence back up.
///
/// It MUST be shorter than the depth being maintained. Poll every 100 ms while
/// holding 50 ms of silence and the ring empties between top-ups, so a feature
/// whose entire purpose is to avoid a stopped clock would instead cause an
/// underrun every gap. Half the depth, bounded so a small depth cannot spin the
/// thread and a large one cannot make it sluggish about picking up a track.
///
/// Fed the CONFIGURED depth, which is a lower bound on the one actually held
/// (`resolve_keepalive_depth_ms` widens it against the ring). Erring toward
/// topping up too often is the safe direction.
///
/// With the keep-alive off, each caller keeps the cadence it always had —
/// `when_off` — so turning the feature off changes nothing about how the
/// writer idles.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn keepalive_poll_interval(depth_ms: u32, when_off: Duration) -> Duration {
    if depth_ms == 0 {
        return when_off;
    }
    Duration::from_millis(u64::from(depth_ms / 2).clamp(5, 100))
}

/// Hold a floor of silence under an idle PCM, if the host asked for it.
///
/// `primed` says whether this stream has already played something. It gates
/// the whole thing, and it is not a detail: the first version ran during the
/// wait for a stream's INITIAL buffer, where nothing had played yet. There is
/// no discontinuity to hide there — the clock was not running — so all it did
/// was start the clock early with a thin floor behind it, and the ring was dry
/// by the time real audio arrived:
///
///   23:25:58.586  Writer thread started
///   23:25:58.987  buffer ready in 400ms, playback starting
///   23:25:59.005  WARN Recovered from PCM error
///
/// Three of those in five minutes, against zero in the whole log before it.
/// The keep-alive bridges a gap BETWEEN things already played; before the
/// first one there is nothing to bridge.
///
/// Best effort otherwise: a device that refuses silence must not take the
/// writer thread down with it. One warning, then we go quiet about it — the
/// idle paths call this many times a second.
/// The two gates on the keep-alive, named so they can be tested.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn would_keep_alive(primed: bool, depth_ms: u32) -> bool {
    primed && depth_ms > 0
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Returns the frames of silence actually queued.
///
/// The count is not decoration. `snd_pcm_delay` reports everything the device
/// is holding, silence included, and the writer subtracts the delay from what
/// it has handed over to decide how much audio a pause has to give back. Silence
/// is not in that replay buffer, so a keep-alive top-up the writer did not count
/// would make it hand back too much — an audible repeat at the resume.
fn keep_dac_awake(stream: &Arc<dyn AudioOut>, cancel: &Arc<AtomicBool>, primed: bool) -> usize {
    let depth_ms = qbz_audio::alsa_direct::dac_keepalive_ms();
    if !would_keep_alive(primed, depth_ms) {
        return 0;
    }
    match stream.write_silence_to_depth(depth_ms, cancel) {
        Ok(frames) => frames,
        Err(e) => {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "[ALSA Direct Engine] DAC keep-alive silence failed ({e}); giving up on it"
                );
            }
            0
        }
    }
}

/// How many frames the decoder pulls from a source in one go.
///
/// Pure batching, with no deadline attached: `Iterator::next` on a boxed source
/// chain costs a virtual call per sample, and this amortises the loop around it.
/// It bears no relation to the ALSA period — that was the OLD writer's problem,
/// when one thread did both jobs and a 8192-frame decode block was 1.5x the
/// whole hardware ring at 44.1 kHz. This thread has no deadline at all.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DECODE_BLOCK_FRAMES: usize = 4096;

/// How long the decoder waits for room before looking again.
///
/// A backstop, not the mechanism: the writer notifies after every chunk it
/// takes. This bounds the cost of a missed notification — `RingLink::notify_space`
/// deliberately does not take the lock, because the writer is real-time — and
/// gives the thread a regular chance to notice `should_stop`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DECODER_SPACE_WAIT: Duration = Duration::from_millis(50);

/// How long the writer sleeps when the decoded ring has run dry.
///
/// Only reached when the decoder is behind, which is the failure the ring
/// exists to prevent — so by the time this runs, a few milliseconds are not
/// what is going wrong.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const WRITER_STARVED_SLEEP: Duration = Duration::from_millis(2);

/// How long an empty ring must stay empty before the writer concludes that
/// nothing more is coming and starts the clock on a partial buffer.
///
/// See `AlsaDirectStream::start_if_prepared`: `start_threshold` is the whole
/// ring, so a stream that will never receive a full one never starts. Short
/// tracks, short tails and a resume with little buffered all land here. Long
/// enough not to fire on a decoder that is merely a moment behind; short enough
/// that nobody notices it before a track begins.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const WRITER_START_ON_IDLE: Duration = Duration::from_millis(120);

/// Absolute ceiling on waiting for the ring to prime before starting anyway.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const WRITER_PRIME_DEADLINE: Duration = Duration::from_secs(3);

/// Decode sources into the ring. Ordinary priority, and free to block.
///
/// This half of the split exists so that the thread which owes ALSA a period
/// every few milliseconds is not also the thread that waits on a socket.
/// `IncrementalStreamingSource::next` runs symphonia inline and parks on the
/// network buffer when the download is behind; that is now invisible to the
/// device for as long as the ring holds out, which is the entire point.
///
/// Holds NO `Arc<AlsaDirectStream>`, and that is a safety property rather than
/// an accident of what it needs — see the comment in `new_alsa_direct`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn alsa_decoder_thread(
    mut producer: HeapProd<f32>,
    link: Arc<RingLink>,
    should_stop: Arc<AtomicBool>,
    source_queue: Arc<SourceQueue<QueuedSource>>,
    channels: u16,
) {
    let channels = usize::from(channels).max(1);
    let mut staging: Vec<f32> = Vec::with_capacity(DECODE_BLOCK_FRAMES * channels);
    let mut current_source: Option<BoxedSampleIter> = None;
    // Samples pushed into the ring that have not yet been counted as whole
    // frames. `push_slice` works in samples and can stop mid-frame, so the
    // frame counter only ever advances by whole frames and the remainder rides
    // along to the next push.
    let mut uncounted_samples: usize = 0;
    // Whether an `EndOfStream` marker is already queued for the current silence.
    // Without it an idle decoder would push one every turn and the writer would
    // report the track finished over and over.
    let mut end_marked = false;

    log::info!("[ALSA Direct Engine] Decoder thread started");

    'thread: loop {
        // UNCONDITIONAL, and first. Before the decode/output split the writer
        // could drain at a natural end and turn playback back on for a late
        // gapless hand-off, sailing past this check into the blocking write
        // below — and a device that has stopped draining never lets that write
        // return. The join in `stop_inner` then waited forever and the audio
        // thread was lost for the life of the process, silently. Nothing sets
        // `is_playing` from in here any more, so the hazard is structural now;
        // keep it that way. `a_stop_completes_promptly_from_any_state` is the
        // regression test.
        if should_stop.load(Ordering::SeqCst) {
            break 'thread;
        }

        if current_source.is_none() {
            match source_queue.wait_for_source(Duration::from_millis(100)) {
                Some(src) => {
                    // Recorded BEFORE the frames it refers to are pushed, so
                    // `at_frame` is the index of this source's first frame, and
                    // `offset_frames` says where in its own track that frame is.
                    link.push_boundary(BoundaryKind::TrackStart {
                        offset_frames: src.start_frame,
                    });
                    current_source = Some(src.iter);
                    end_marked = false;
                    log::info!("[ALSA Direct Engine] Decoder acquired a source");
                }
                None => {
                    // Nothing queued, and the previous source is exhausted:
                    // tell the writer that the tail it is holding is the last
                    // of it. Once, not once per turn.
                    if !end_marked {
                        link.push_boundary(BoundaryKind::EndOfStream);
                        end_marked = true;
                    }
                    continue 'thread;
                }
            }
        }

        // Pull a block. Decode and any network wait happen here, off the
        // real-time thread.
        staging.clear();
        let source = current_source.as_mut().expect("source present");
        let mut source_ended = false;
        for _ in 0..DECODE_BLOCK_FRAMES * channels {
            match source.next() {
                Some(sample) => staging.push(sample),
                None => {
                    source_ended = true;
                    break;
                }
            }
        }

        // Push the WHOLE block, however many attempts it takes. `push_slice`
        // returns short when the ring is full, and dropping that remainder
        // would be dropping audio — 93 ms of it per short write at CD rate.
        let mut offset = 0usize;
        while offset < staging.len() {
            if should_stop.load(Ordering::SeqCst) {
                break 'thread;
            }
            let pushed = producer.push_slice(&staging[offset..]);
            if pushed == 0 {
                // Ring full. This is the NORMAL steady state once playback is
                // established — a full ring is the whole objective — so it is a
                // wait, not a problem.
                if !link.wait_for_space(DECODER_SPACE_WAIT) {
                    break 'thread;
                }
                continue;
            }
            offset += pushed;
            uncounted_samples += pushed;
            let whole_frames = uncounted_samples / channels;
            if whole_frames > 0 {
                link.add_produced(whole_frames as u64);
                uncounted_samples -= whole_frames * channels;
            }
        }

        if source_ended {
            // A gapless transition is simply the decoder changing sources
            // mid-fill: the ring does not care where its samples came from, and
            // the writer never sees a seam.
            match source_queue.try_pop() {
                Some(next_src) => {
                    log::info!("[ALSA Direct Engine] Decoder handing over to the next source");
                    link.push_boundary(BoundaryKind::TrackStart {
                        offset_frames: next_src.start_frame,
                    });
                    current_source = Some(next_src.iter);
                    end_marked = false;
                }
                None => current_source = None,
            }
        }
    }

    link.close();
    log::info!("[ALSA Direct Engine] Decoder thread finished");
}

/// Drain the ring into ALSA. Real-time, and must never block on anything but
/// the device.
///
/// Everything that can wait — the network, the disk, symphonia — is on the
/// decoder thread. What is left here is: copy from a lock-free ring, scale,
/// convert, hand to ALSA. That is what makes it safe to run this thread at
/// `SCHED_FIFO` (see `qbz_audio::rt`), and it is why nothing which can wait may
/// ever be added to it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn alsa_writer_thread(
    mut consumer: HeapCons<f32>,
    stream: Arc<dyn AudioOut>,
    link: Arc<RingLink>,
    is_playing: Arc<AtomicBool>,
    should_stop: Arc<AtomicBool>,
    position_frames: Arc<AtomicU64>,
    source_transition: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    channels: u16,
) {
    qbz_audio::rt::promote_writer_thread_and_log();

    let channels = usize::from(channels).max(1);
    // One or two periods per write, taken from what the DRIVER granted rather
    // than a constant. The old `CHUNK_FRAMES = 8192` was 185.8 ms at 44.1 kHz
    // against a 125 ms ring — a work quantum half again as large as the entire
    // buffer it was feeding, so the ring was structurally drained to empty on
    // every iteration.
    let chunk_frames = writer_chunk_frames(stream.period_frames(), stream.buffer_frames());
    let chunk_samples = chunk_frames * channels;
    let alsa_ring_samples = stream.buffer_frames() * channels;

    // Popped from the ring, pre-gain, not yet all accepted by the device.
    let mut pending: Vec<f32> = Vec::with_capacity(chunk_samples);
    let mut pending_offset = 0usize;
    // Post-gain scratch handed to ALSA. Gain is applied per attempt so a volume
    // change takes effect within one period rather than one ring.
    let mut out: Vec<f32> = Vec::with_capacity(chunk_samples);
    // The last `alsa_ring_samples` handed over, pre-gain, so a pause can give
    // back whatever the device was still holding when we dropped it.
    let mut replay: VecDeque<f32> = VecDeque::with_capacity(alsa_ring_samples + chunk_samples);
    // Frames returned by a pause, to be handed again before the ring.
    let mut resume_pending: Vec<f32> = Vec::new();
    let mut resume_offset = 0usize;
    // Whether `pending` came from `resume_pending` rather than from the ring.
    // Replayed frames must NOT be counted as newly handed: `link` was told
    // about them the first time round, and counting them twice would walk
    // `frames_handed` past the boundary markers and fire the next track's
    // transition early.
    let mut pending_is_replay = false;
    // Silence frames handed since the last real audio. `snd_pcm_delay` counts
    // them too, and they are not in `replay`, so a pause must subtract them
    // before deciding how much audio to give back.
    let mut silence_since_audio = 0usize;

    let mut was_playing = false;
    let mut primed = false;
    let mut first_track_seen = false;
    let mut segment_base_frames: u64 = 0;
    // Where the current source sits in its own TRACK. Non-zero after a seek or
    // a resume, and what turns a source position into a track position.
    let mut segment_offset_frames: u64 = 0;
    let mut segment_position: u64 = 0;
    let mut ring_empty_since: Option<std::time::Instant> = None;
    let started_at = std::time::Instant::now();
    let mut finished_reported = false;

    log::info!(
        "[ALSA Direct Engine] Writer thread started — chunk {chunk_frames} frames, \
         hardware ring {} frames",
        stream.buffer_frames()
    );

    'thread: loop {
        if should_stop.load(Ordering::SeqCst) {
            break 'thread;
        }

        let playing = is_playing.load(Ordering::SeqCst);

        // ---- pause edge -------------------------------------------------
        if was_playing && !playing {
            // Read the delay BEFORE dropping: afterwards the PCM is in SETUP
            // and `snd_pcm_delay` returns EBADFD.
            let delay = stream.delay_frames();
            let unplayed_audio = delay
                .saturating_sub(silence_since_audio)
                .min(replay.len() / channels);
            if let Err(e) = stream.discard_queued() {
                log::warn!("[ALSA Direct Engine] pause could not reset the pcm: {e}");
            }
            // Take back exactly what the device was holding and never played,
            // then whatever had been popped from the ring but not yet handed
            // over. Without this a pause would silently swallow up to a whole
            // ring — a full second on the buffer a Pi is told to use.
            //
            // ORDER MATTERS: the replayed frames come BEFORE the un-handed
            // remainder, because that is the order they were decoded in.
            // Leaving the remainder in `pending` would have played it first —
            // later audio ahead of earlier audio, at every resume.
            resume_pending.clear();
            let take = (unplayed_audio * channels).min(replay.len());
            resume_pending.extend(replay.iter().skip(replay.len() - take).copied());
            resume_pending.extend_from_slice(&pending[pending_offset.min(pending.len())..]);
            resume_offset = 0;
            pending.clear();
            pending_offset = 0;
            replay.clear();
            silence_since_audio = 0;
            log::info!(
                "[ALSA Direct Engine] Paused — {unplayed_audio} frames taken back from the device"
            );
        }
        was_playing = playing;

        if !playing {
            // A gap with the clock stopped is a click at both ends of it.
            // Uncounted on purpose: the pause edge above already emptied the
            // device and reset the accounting, and resume hands its own frames
            // back from `resume_pending` regardless of what silence is queued.
            let _ = keep_dac_awake(&stream, &should_stop, primed);
            thread::sleep(keepalive_poll_interval(
                qbz_audio::alsa_direct::dac_keepalive_ms(),
                Duration::from_millis(50),
            ));
            continue 'thread;
        }

        // ---- get something to hand over ---------------------------------
        if pending_offset >= pending.len() {
            pending.clear();
            pending_offset = 0;
            if resume_offset < resume_pending.len() {
                // Whatever the pause took back goes out first.
                let take = (resume_pending.len() - resume_offset).min(chunk_samples);
                pending.extend_from_slice(&resume_pending[resume_offset..resume_offset + take]);
                resume_offset += take;
                pending_is_replay = true;
                if resume_offset >= resume_pending.len() {
                    resume_pending.clear();
                    resume_offset = 0;
                }
            } else {
                pending.resize(chunk_samples, 0.0);
                let popped = consumer.pop_slice(&mut pending);
                // How full the ring WAS, not how much this read took.
                //
                // `pop_slice` returns at most one chunk, so recording its
                // result measured the chunk size and nothing else — a constant,
                // reported every tick, that said the same thing about a ring
                // brimming and a ring one frame from empty. The occupancy is
                // produced-minus-handed, which is the figure that predicts a
                // glitch; an xrun counter only records one after it has been
                // heard.
                //
                // Two relaxed atomic stores. The real-time thread may not log,
                // so somebody else decides whether this is worth complaining
                // about.
                let fill = link.frames_produced().saturating_sub(link.frames_handed());
                link.record_fill(fill, popped == 0);
                pending.truncate(popped);
                pending_is_replay = false;
            }
        }

        if pending.is_empty() {
            // Nothing to hand over. Either the decoder is momentarily behind,
            // or there is genuinely nothing more coming — and those need
            // opposite responses, so they are told apart by how long it lasts.
            let empty_since = *ring_empty_since.get_or_insert_with(std::time::Instant::now);
            let idle_long_enough = empty_since.elapsed() >= WRITER_START_ON_IDLE;
            let priming_too_long = !primed && started_at.elapsed() >= WRITER_PRIME_DEADLINE;
            if idle_long_enough || priming_too_long {
                // `start_threshold` is the whole ring, so a stream that will
                // never receive a full one never starts — no sound, no error.
                if let Err(e) = stream.start_if_prepared() {
                    log::warn!("[ALSA Direct Engine] could not start an idle stream: {e}");
                }
            }
            // Counted, because this runs while PLAYING: a pause taken right
            // after a top-up must not mistake this silence for audio the device
            // still owes us.
            silence_since_audio += keep_dac_awake(&stream, &should_stop, primed);
            thread::sleep(WRITER_STARVED_SLEEP);
            // NO `continue` HERE, and that is the whole point.
            //
            // This branch runs precisely when the ring has run dry — which is
            // exactly when the end of the stream is about to be marked. Skipping
            // the boundary handling below meant the writer handed the last frame
            // of the last track, found the ring empty, and then span in this
            // branch forever: the `EndOfStream` the decoder pushed a moment
            // later was never taken, so the drain never ran, `is_playing` was
            // never cleared, and the player never learned the track had
            // finished. Gapless masked it — the next source arrives before the
            // ring empties — so it only bit at the end of a queue, which is
            // also the hardest case to notice.
        } else {
            ring_empty_since = None;

            // ---- scale and hand over ------------------------------------
            out.clear();
            out.extend_from_slice(&pending[pending_offset..]);
            // Software volume, applied at the last possible moment so a change
            // takes effect within one period instead of one ring. Unity is the
            // bit-perfect case and costs one atomic load rather than a multiply per
            // sample. Attenuation only, so this cannot clip.
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            if gain != 1.0 {
                for sample in out.iter_mut() {
                    *sample *= gain;
                }
            }

            let accepted_frames = match stream.write_f32(&out, &should_stop) {
                Ok(frames) => frames,
                Err(e) => {
                    log::error!("[ALSA Direct Engine] Write failed: {e}");
                    break 'thread;
                }
            };
            let accepted_samples = accepted_frames * channels;
            if accepted_frames > 0 {
                primed = true;
                silence_since_audio = 0;
                // Keep the pre-gain copy so a pause can give it back at whatever
                // volume is current when playback resumes.
                replay.extend(
                    pending[pending_offset..(pending_offset + accepted_samples).min(pending.len())]
                        .iter()
                        .copied(),
                );
                while replay.len() > alsa_ring_samples {
                    replay.pop_front();
                }
                pending_offset += accepted_samples;
                if !pending_is_replay {
                    link.add_handed(accepted_frames as u64);
                    // Tell the decoder there is room. Takes no lock — this is the
                    // real-time thread.
                    link.notify_space();
                }
            }
        }

        // ---- markers the decoder left in the stream -----------------------
        //
        // BEFORE the position is published, so the offset a `TrackStart` carries
        // is already in effect for the first position of its segment. The other
        // order publishes one chunk at the previous segment's offset, and
        // everything downstream keyed on the absolute position sees it.
        while let Some(boundary) = link.take_due_boundary() {
            match boundary.kind {
                BoundaryKind::TrackStart { offset_frames } => {
                    segment_base_frames = boundary.at_frame;
                    segment_offset_frames = offset_frames;
                    segment_position = 0;
                    position_frames.store(offset_frames, Ordering::SeqCst);
                    finished_reported = false;
                    if first_track_seen {
                        log::info!("[ALSA Direct Engine] Gapless transition to the next source");
                        source_transition.store(true, Ordering::SeqCst);
                    }
                    first_track_seen = true;
                }
                BoundaryKind::EndOfStream => {
                    if finished_reported {
                        continue;
                    }
                    // A marker can be STALE by the time the writer reaches it.
                    //
                    // The decoder writes it the moment it runs out of sources,
                    // but the ring is seconds deep, so the writer arrives here
                    // seconds later — and a late gapless hand-off may already
                    // have queued a `TrackStart` behind it, with the next
                    // track's audio sitting in the ring. Acting on the marker
                    // then would drain the device and stop playback on a track
                    // that was about to play: the writer would park on the
                    // pause gate holding a source, and the player would report
                    // the next track at its full duration having never heard a
                    // note of it.
                    if link.has_queued_boundaries() {
                        log::info!(
                            "[ALSA Direct Engine] End-of-stream superseded by a late hand-off"
                        );
                        continue;
                    }
                    finished_reported = true;
                    log::info!("[ALSA Direct Engine] End of stream reached");
                    // The tail is almost never a whole ring, so the clock has
                    // to be told to run for it.
                    if let Err(e) = stream.start_if_prepared() {
                        log::warn!("[ALSA Direct Engine] could not start the tail: {e}");
                    }
                    if qbz_audio::alsa_direct::dac_keepalive_ms() > 0 {
                        // Draining stops the clock, which is the edge the
                        // keep-alive exists to avoid; the silence pushes the
                        // tail out just as well.
                        silence_since_audio += keep_dac_awake(&stream, &should_stop, primed);
                    } else if let Err(e) = stream.drain() {
                        log::warn!("[ALSA Direct Engine] Drain failed: {e}");
                    }
                    is_playing.store(false, Ordering::SeqCst);
                    was_playing = false;
                }
            }
        }

        // ---- position, from the audio clock ------------------------------
        //
        // `handed - delay` is what the DAC has actually CONVERTED, not what we
        // have written; the two differ by the whole hardware ring, up to a full
        // second on the buffer a Pi is told to use.
        //
        // `segment_offset_frames` is what makes this a TRACK position rather
        // than a source position. A seek rebuilds the engine around a source
        // whose first frame is already minutes into the song, and without the
        // offset this reported "seconds since the seek" — which the player then
        // adopted as its clock, so the QConnect buffering latch (released only
        // once the clock passes the offset that was seeked to) never released,
        // and the controller span forever on a track that was audibly playing.
        let played_total = link
            .frames_handed()
            .saturating_sub(stream.delay_frames() as u64);
        // Monotonic within a segment: a resume hands frames back that were
        // already counted, so `delay` briefly exceeds what `handed` accounts
        // for and the difference dips. Holding the last value flat for those
        // few hundred milliseconds is closer to the truth than going backwards.
        segment_position =
            segment_played_frames(played_total, segment_base_frames, segment_position);
        position_frames.store(
            segment_offset_frames.saturating_add(segment_position),
            Ordering::SeqCst,
        );
    }

    is_playing.store(false, Ordering::SeqCst);
    link.close();
    log::info!("[ALSA Direct Engine] Writer thread finished");
}

/// How far into the CURRENT SOURCE the DAC has got, monotonically.
///
/// `played_total` counts frames the device has converted since the stream
/// opened; `segment_base` is where the current source began in that count. The
/// `previous` floor is what keeps it monotonic: a resume hands back frames the
/// device had already been given, so `snd_pcm_delay` briefly accounts for more
/// than `frames_handed` does and the difference dips. Holding the last value
/// flat for those few hundred milliseconds is closer to the truth than letting
/// the reported position walk backwards.
///
/// Add the source's own track offset to turn this into a TRACK position.
pub(crate) fn segment_played_frames(played_total: u64, segment_base: u64, previous: u64) -> u64 {
    previous.max(played_total.saturating_sub(segment_base))
}

/// Frames the writer hands over in one go, from the geometry the driver granted.
///
/// Two periods: enough that the thread is not woken more often than it needs to
/// be, small enough that it is a fraction of the ring rather than a multiple of
/// it. The clamp is a guard against a driver reporting something absurd, not a
/// tuning knob — anything inside it is the device's own answer.
///
/// The bug this replaces: a hard-coded 8192 frames, which is 185.8 ms at
/// 44.1 kHz, against `buffer_frames_for(44100)` = 5512 frames = 125 ms. At the
/// most common sample rate the writer's work quantum was 1.5x the entire
/// hardware buffer, so the ring was drained toward empty on every iteration and
/// there was no margin for anything to go wrong in.
pub(crate) fn writer_chunk_frames(period_frames: usize, buffer_frames: usize) -> usize {
    const MIN_CHUNK: usize = 256;
    const MAX_CHUNK: usize = 16_384;
    let from_period = period_frames.saturating_mul(2);
    // Never more than a quarter of the ring, whatever the period says: the
    // quantum has to leave room for the device to be draining while we work.
    let ceiling = (buffer_frames / 4).max(MIN_CHUNK);
    from_period.clamp(MIN_CHUNK, ceiling.min(MAX_CHUNK))
}

/// Single long-lived feeder thread for JACK (#263 Tier 3).
///
/// Mirrors `alsa_writer_thread`, but writes graph-rate interleaved STEREO f32
/// into the JACK client's lock-free ring buffer via `JackStream::write_f32`
/// (the RT process callback drains it), pacing itself when the ring is full.
/// Sources are resampled to the graph rate + stereo at `append` time.
#[cfg(target_os = "linux")]
fn jack_feeder_thread(
    stream: Arc<JackStream>,
    is_playing: Arc<AtomicBool>,
    should_stop: Arc<AtomicBool>,
    position_frames: Arc<AtomicU64>,
    source_queue: Arc<SourceQueue<BoxedSampleIter>>,
    source_transition: Arc<AtomicBool>,
) {
    const CHUNK_FRAMES: usize = 4096;
    const CHANNELS: usize = 2;
    let chunk_samples = CHUNK_FRAMES * CHANNELS;
    let mut buffer_f32: Vec<f32> = Vec::with_capacity(chunk_samples);
    let mut current_source: Option<BoxedSampleIter> = None;
    let mut total_frames: u64 = 0;

    log::info!("[JACK Engine] Feeder thread started");

    'thread: loop {
        if should_stop.load(Ordering::SeqCst) {
            break 'thread;
        }
        if current_source.is_none() {
            match source_queue.wait_for_source(Duration::from_millis(100)) {
                Some(src) => {
                    current_source = Some(src);
                    total_frames = 0;
                    position_frames.store(0, Ordering::SeqCst);
                }
                None => continue 'thread,
            }
        }
        while !is_playing.load(Ordering::SeqCst) {
            if should_stop.load(Ordering::SeqCst) {
                break 'thread;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        buffer_f32.clear();
        let source = current_source.as_mut().unwrap();
        let mut source_ended = false;
        for _ in 0..chunk_samples {
            match source.next() {
                Some(s) => buffer_f32.push(s),
                None => {
                    source_ended = true;
                    break;
                }
            }
        }

        // Write to the ring, paced: write_f32 returns frames accepted; a full
        // ring returns fewer (or 0) and we wait for the RT callback to drain.
        let mut off_samples = 0usize;
        while off_samples < buffer_f32.len() {
            if should_stop.load(Ordering::SeqCst) {
                break 'thread;
            }
            let frames = stream.write_f32(&buffer_f32[off_samples..]);
            if frames == 0 {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            off_samples += frames * CHANNELS;
            total_frames += frames as u64;
            position_frames.store(total_frames, Ordering::SeqCst);
        }

        if source_ended {
            match source_queue.try_pop() {
                Some(next_src) => {
                    current_source = Some(next_src);
                    total_frames = 0;
                    position_frames.store(0, Ordering::SeqCst);
                    source_transition.store(true, Ordering::SeqCst);
                }
                None => {
                    current_source = None;
                    is_playing.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    is_playing.store(false, Ordering::SeqCst);
    log::info!("[JACK Engine] Feeder thread finished");
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[cfg(test)]
mod track_position_tests {
    use super::segment_played_frames;

    const RATE: u64 = 44_100;

    /// The regression this exists for, reported from a Pi.
    ///
    /// Seek to 2:46 of a song. The engine is rebuilt around a source whose
    /// first decoded frame is already 166 seconds in, so the writer's own
    /// counters restart at zero — but the position it publishes is adopted by
    /// the player as the position within the TRACK. Reported as
    /// frames-since-the-seek it read 0, 1, 2, 3 seconds, and the QConnect
    /// buffering latch — which releases only once the player's clock passes the
    /// offset that was seeked to — never released. The controller sat spinning
    /// with a frozen clock on a track that was audibly playing, and because the
    /// report is suppressed while buffering, a track change went unreported too.
    ///
    /// The offset is added by the caller; this pins the two halves together.
    #[test]
    fn a_seeked_source_reports_its_position_within_the_track() {
        let offset = 166 * RATE; // the seek target, in frames
        let base = 0; // this source starts the stream
                      // One second of audio has been converted since the seek.
        let played = RATE;

        let within_segment = segment_played_frames(played, base, 0);
        let track_position = offset + within_segment;

        assert_eq!(within_segment, RATE, "one second into the source");
        assert_eq!(track_position / RATE, 167, "which is 2:47 of the track");
        assert!(
            track_position > offset,
            "must pass the seek offset, or the buffering latch never releases"
        );
    }

    /// A gapless hand-off starts the next track at zero, partway through the
    /// stream — so the base moves and the offset does not.
    #[test]
    fn a_gapless_handoff_restarts_the_position_at_zero() {
        let base = 300 * RATE; // the new source began 300 s into the stream
        let played = 302 * RATE; // and two seconds of it have played
        assert_eq!(segment_played_frames(played, base, 0) / RATE, 2);
    }

    /// A resume re-hands frames the device had already taken, so the
    /// delay-derived figure dips for a few hundred milliseconds. The reported
    /// position must sit still rather than walk backwards.
    #[test]
    fn the_position_never_goes_backwards_across_a_resume() {
        let base = 0;
        let settled = segment_played_frames(10 * RATE, base, 0);
        // The dip: `delay` now accounts for frames `handed` does not.
        let during_resume = segment_played_frames(9 * RATE, base, settled);
        assert_eq!(during_resume, settled, "position walked backwards");
        // And it resumes advancing once the re-handed frames drain.
        assert_eq!(
            segment_played_frames(11 * RATE, base, during_resume),
            11 * RATE
        );
    }

    /// A base ahead of the play count (the instant a boundary lands) must read
    /// as zero rather than underflow.
    #[test]
    fn a_boundary_that_has_not_been_reached_yet_reads_as_zero() {
        assert_eq!(segment_played_frames(100, 500, 0), 0);
    }
}

#[cfg(test)]
mod writer_chunk_tests {
    use super::writer_chunk_frames;

    /// The bug this function exists to make impossible.
    ///
    /// The writer used to hand ALSA a hard-coded 8192 frames. At 44.1 kHz that
    /// is 185.8 ms, against a default hardware ring of `44100/8` = 5512 frames
    /// = 125 ms — so the work quantum was one and a half times the entire
    /// buffer it was feeding, and the ring was structurally drained toward
    /// empty on every iteration with no margin for anything to go wrong in.
    /// CD rate was the worst case, and it is the common one.
    #[test]
    fn the_write_quantum_is_never_more_than_a_quarter_of_the_ring() {
        // Rate-derived defaults at each rate, with the driver's conventional
        // four periods per buffer.
        for buffer in [5_512usize, 24_000, 96_000, 44_100, 192_000] {
            let period = buffer / 4;
            let chunk = writer_chunk_frames(period, buffer);
            assert!(
                chunk * 4 <= buffer.max(1024),
                "chunk {chunk} is more than a quarter of a {buffer}-frame ring"
            );
        }
    }

    /// Two periods where the ring is deep enough in periods to allow it, one
    /// where it is not. With the four periods per buffer this code asks for,
    /// the quarter-ring rule is the one that binds and a chunk is exactly one
    /// period — which is also the classic ALSA idiom, and what `avail_min`
    /// wakes the writer for.
    #[test]
    fn a_chunk_is_up_to_two_periods_and_never_more_than_a_quarter_ring() {
        // 1 s ring at 44.1 kHz in four periods: two periods would be half the
        // ring, so the quarter-ring ceiling wins and we get one period.
        assert_eq!(writer_chunk_frames(11_025, 44_100), 11_025);
        // The same ring in sixteen periods: two of them fit inside a quarter.
        assert_eq!(writer_chunk_frames(2_756, 44_100), 5_512);
    }

    /// A driver that reports something absurd must not produce a zero-length
    /// or gigantic write; both would wedge the writer rather than glitch it.
    #[test]
    fn a_nonsense_geometry_still_yields_a_usable_chunk() {
        assert!(writer_chunk_frames(0, 0) >= 256);
        assert!(writer_chunk_frames(0, 5_512) >= 256);
        let huge = writer_chunk_frames(1_000_000, 8_000_000);
        assert!((256..=16_384).contains(&huge), "got {huge}");
    }
}

#[cfg(test)]
mod keepalive_cadence_tests {
    use super::keepalive_poll_interval;
    use std::time::Duration;

    #[test]
    fn the_keepalive_is_topped_up_faster_than_it_drains() {
        // The bug this guards: polling every 100 ms while holding 50 ms of
        // silence empties the ring between top-ups, turning a feature meant to
        // prevent a stopped clock into an underrun every gap.
        for depth_ms in [10, 20, 50, 100, 200, 500] {
            let interval = keepalive_poll_interval(depth_ms, Duration::from_millis(100));
            assert!(
                interval < Duration::from_millis(u64::from(depth_ms)),
                "{depth_ms} ms of silence topped up only every {interval:?}"
            );
        }
    }

    #[test]
    fn a_small_depth_does_not_spin_and_a_large_one_stays_responsive() {
        // A 10 ms depth would ask for 5 ms; the floor keeps it there rather
        // than lower.
        assert_eq!(
            keepalive_poll_interval(10, Duration::from_millis(100)),
            Duration::from_millis(5)
        );
        // A 500 ms depth is capped, so the thread still notices a queued
        // track promptly.
        assert_eq!(
            keepalive_poll_interval(500, Duration::from_millis(100)),
            Duration::from_millis(100)
        );
    }

    /// The gate that the first version was missing. `keep_dac_awake` returns
    /// immediately when nothing has played, so the wait for a stream's initial
    /// buffer no longer starts the clock with an empty ring behind it.
    #[test]
    fn nothing_is_written_before_the_stream_has_played_anything() {
        // `keep_dac_awake` needs a live stream to exercise fully; what is
        // testable here — and what regressed — is that the depth lookup is
        // never even reached unless primed. Kept as a documented invariant
        // next to the cadence it shares a bug history with.
        assert!(
            !super::would_keep_alive(false, 50),
            "an unprimed stream must not be clocked"
        );
        assert!(
            !super::would_keep_alive(true, 0),
            "off is off even once primed"
        );
        assert!(super::would_keep_alive(true, 50));
    }

    #[test]
    fn off_leaves_each_caller_the_cadence_it_always_had() {
        assert_eq!(
            keepalive_poll_interval(0, Duration::from_millis(100)),
            Duration::from_millis(100)
        );
        assert_eq!(
            keepalive_poll_interval(0, Duration::from_millis(50)),
            Duration::from_millis(50)
        );
    }
}

/// Behaviour tests for the ALSA-direct engine, driven against a virtual device.
///
/// # Why these exist
///
/// Every bug this engine has shipped was a contract between components rather
/// than a fault in any one function — the position reported after a seek, what
/// a pause does to committed audio, whether a short tail plays at all. Unit
/// tests of the individual functions passed through all of them. These tests
/// run the REAL decoder thread, the REAL writer thread and the REAL ring
/// against `VirtualAudioOut`, and assert the behaviour a listener or a
/// controller would observe.
///
/// # The rules for adding to this
///
/// - **Every bug found on hardware gets a case here before it is fixed.** A
///   suite nobody adds to is dead weight; the discipline is the point.
/// - Assert what is OBSERVABLE — position, frames played, transitions — never
///   internal counters. The internals are what we want to be free to change.
/// - Assert inequalities and bounds, never exact timings. These run on a
///   virtual clock on a shared CI box.
///
/// # What they cannot tell you
///
/// Nothing here says the audio is good. Clicks, xruns on real hardware, the
/// stop ramp, `start_threshold` against a real driver, RT scheduling — none of
/// it is visible to a virtual device. Green here means the state machine is
/// right, not that the Pi sounds right.
#[cfg(test)]
mod engine_behaviour_tests {
    use super::*;
    use qbz_audio::VirtualAudioOut;
    use rodio::buffer::SamplesBuffer;
    use std::time::{Duration, Instant};

    const RATE: u32 = 44_100;
    const CHANNELS: u16 = 2;
    /// Ring short enough that the engine primes quickly, long enough to be
    /// realistic about the start threshold.
    const RING_MS: u32 = 200;
    /// Virtual clock multiplier: a 3-second track plays in ~30 ms.
    const SPEED: f64 = 100.0;

    /// A constant-amplitude source, so the tape can be inspected for gaps.
    fn tone(secs: f32, amplitude: f32) -> SamplesBuffer {
        let frames = (RATE as f32 * secs) as usize;
        SamplesBuffer::new(
            std::num::NonZero::new(CHANNELS).unwrap(),
            std::num::NonZero::new(RATE).unwrap(),
            vec![amplitude; frames * usize::from(CHANNELS)],
        )
    }

    fn device() -> Arc<VirtualAudioOut> {
        device_at(SPEED)
    }

    /// A device on a slower virtual clock.
    ///
    /// `SPEED` is 100x, which asks the decoder thread to sustain a hundred
    /// times real-time decode. That is fine for a state-machine assertion and
    /// wrong for an UNDERRUN assertion: on a loaded box — the whole suite
    /// running in parallel — a 10 ms scheduling gap eats a full second of
    /// virtual audio and the ring legitimately runs dry. The dryness is the
    /// harness being honest about a starved writer, not the engine misbehaving,
    /// so a test that counts underruns runs somewhere the decoder can keep up.
    fn device_at(speed: f64) -> Arc<VirtualAudioOut> {
        Arc::new(VirtualAudioOut::new(RATE, CHANNELS, RING_MS, speed))
    }

    /// Poll until `f` holds or the deadline passes. Returns whether it held.
    ///
    /// Generous by design: these run on a virtual clock on a shared CI box, and
    /// a test that is flaky under load is worse than no test.
    fn wait_for(what: &str, f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        eprintln!("timed out waiting for: {what}");
        false
    }

    /// THE REGRESSION. Reported from a Pi: seek deep into a song and the
    /// controller spins forever, its clock frozen at the seek point, while the
    /// music plays on perfectly.
    ///
    /// The cause was that the engine reported frames-since-THIS-SOURCE-started.
    /// A seek rebuilds the engine around a source whose first decoded frame is
    /// already 166 seconds into the song, so the position restarted at zero —
    /// and the QConnect buffering latch, which releases only once the clock
    /// passes the offset that was seeked to, never released. Position reporting
    /// is suppressed while buffering, so the clock froze too, and the track
    /// change that followed went unreported.
    ///
    /// The contract: a source appended with an offset reports its position
    /// within the TRACK.
    #[test]
    fn a_seeked_source_reports_its_position_within_the_track() {
        const SEEK_TO: u64 = 166;
        let out = device();
        let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
        engine.append(tone(3.0, 0.25), SEEK_TO).expect("append");

        assert!(
            wait_for("the position to pass the seek offset", || {
                engine.position_secs().is_some_and(|p| p > SEEK_TO)
            }),
            "position never passed {SEEK_TO}s — this is the bug: a controller \
             waiting for the clock to reach the offset it asked for will wait \
             forever, spinning, on a track that is audibly playing"
        );

        // And it keeps climbing from there rather than wrapping back.
        let seen = engine.position_secs().expect("a position");
        assert!(
            wait_for("the position to keep climbing", || {
                engine.position_secs().is_some_and(|p| p > seen)
            }),
            "position stopped at {seen}s"
        );
        assert!(
            engine.position_secs().expect("a position") >= SEEK_TO,
            "the position must never fall back below the seek offset"
        );
        engine.stop();
    }

    /// The position must never be observed BELOW the offset it was appended
    /// at — not merely "reaches it eventually".
    ///
    /// That distinction is the whole contract as far as the QConnect buffering
    /// latch is concerned: it releases when the clock passes where the stream
    /// opened, so a clock that starts at zero and climbs reads as "still
    /// loading" for as long as the climb takes, and the controller spins.
    /// Observed on the Pi as `clock at 15217 ms, BELOW the 51000 ms this
    /// stream opened at`.
    #[test]
    fn a_seeked_source_is_never_reported_below_its_offset() {
        const SEEK_TO: u64 = 166;
        let out = device();
        let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
        engine.append(tone(3.0, 0.25), SEEK_TO).expect("append");

        assert!(wait_for("playback to start", || out.frames_played() > 0));
        // Sample repeatedly across the start of playback: a clock that began at
        // zero would be caught here even though it later climbs past SEEK_TO.
        for _ in 0..50 {
            if let Some(p) = engine.position_secs() {
                assert!(
                    p >= SEEK_TO,
                    "position {p}s is below the {SEEK_TO}s offset — the clock is \
                     source-relative, and the controller will spin until it catches up"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        engine.stop();
    }

    /// A track played from the start reports from zero, not from some offset
    /// left over from whatever the engine did last.
    #[test]
    fn a_source_played_from_the_start_reports_from_zero() {
        let out = device();
        let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
        engine.append(tone(2.0, 0.25), 0).expect("append");

        assert!(wait_for("playback to start", || out.frames_played() > 0));
        assert!(
            engine.position_secs().expect("a position") < 2,
            "a track played from the start cannot already be seconds in"
        );
        engine.stop();
    }

    /// A gapless hand-off: the next track's audio must follow the first with no
    /// silence between, and the position must restart for the new track.
    #[test]
    fn a_gapless_handoff_is_seamless_and_restarts_the_position() {
        let out = device();
        out.record();
        let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
        // Two adjacent tracks at different amplitudes, so the join is findable
        // in the tape.
        engine.append(tone(0.5, 0.25), 0).expect("first");
        engine.append(tone(0.5, 0.75), 0).expect("second");

        assert!(
            wait_for("the transition to be signalled", || engine
                .take_source_transition()),
            "the engine never reported moving to the second source"
        );
        assert!(wait_for("both tracks to finish", || engine.empty()));

        // The tape must contain both amplitudes and NO silence between them:
        // a gap would show up as a run of zeros where the join is.
        let tape = out.recorded();
        assert!(!tape.is_empty(), "nothing was handed to the device");
        let first = tape.iter().filter(|s| (**s - 0.25).abs() < 1e-6).count();
        let second = tape.iter().filter(|s| (**s - 0.75).abs() < 1e-6).count();
        assert!(
            first > 0 && second > 0,
            "one of the two tracks never played"
        );

        // Everything between the first sample and the last must be audio. The
        // stop ramp and pad are at the very end, so look only at the join.
        let join = tape
            .iter()
            .position(|s| (*s - 0.75).abs() < 1e-6)
            .expect("the second track is in the tape");
        let silent_before_join = tape[..join].iter().filter(|s| **s == 0.0).count();
        assert_eq!(
            silent_before_join, 0,
            "{silent_before_join} silent samples before the join — the hand-off is not gapless"
        );
        engine.stop();
    }

    /// The whole point of the exercise: a track must be AUDIBLE in full.
    ///
    /// `start_threshold` is set to the entire hardware ring, so a stream that
    /// never receives a full ring never starts clocking — with no error and no
    /// log. A track shorter than the ring, or the tail of any track, lands
    /// exactly there. This asserts every frame produced actually played.
    #[test]
    fn a_track_shorter_than_the_hardware_ring_still_plays_in_full() {
        let out = device();
        // Deliberately shorter than the ring, which is the trap.
        let track_frames = (out.buffer_frames() / 2) as u64;
        let secs = track_frames as f32 / RATE as f32;

        let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
        engine.append(tone(secs, 0.25), 0).expect("append");

        assert!(
            wait_for("the short track to finish", || engine.empty()),
            "a track shorter than the ring never finished — it probably never started"
        );
        assert!(
            out.frames_played() >= track_frames,
            "only {} of {track_frames} frames were ever played: the clock never started on a \
             partial ring, which is silence with no error",
            out.frames_played()
        );
        engine.stop();
    }

    /// A pause must stop the audio promptly and lose nothing.
    ///
    /// Before the decoder/writer split a pause simply stopped writing: the
    /// device played out whatever was queued — up to a whole second on the
    /// buffer a Pi is told to use — and then underran, so every pause ended in
    /// a click after a second of audio nobody asked for. The fix discards the
    /// queue and hands those frames back on resume, which must not lose or
    /// duplicate them.
    #[test]
    fn a_pause_gives_back_what_the_device_had_not_played() {
        let out = device();
        let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
        engine.append(tone(3.0, 0.25), 0).expect("append");

        assert!(wait_for("playback to start", || out.frames_played() > 0));
        let before = engine.position_secs().expect("a position");
        engine.pause();

        // The device stops owing anything almost at once: the queue is gone,
        // not played out.
        assert!(
            wait_for("the device to be emptied", || out.delay_frames() == 0),
            "the device still held audio well after the pause — it is playing out the queue"
        );

        // And the position does not run on while paused.
        std::thread::sleep(Duration::from_millis(30));
        let during = engine.position_secs().expect("a position");
        assert!(
            during < before + 2,
            "the clock ran on during a pause: {before}s -> {during}s"
        );

        engine.play();
        assert!(
            wait_for("playback to resume", || engine
                .position_secs()
                .is_some_and(|p| p >= during)),
            "playback did not resume"
        );
        engine.stop();
    }

    /// Reaching the end of the queue must be reported once, and must leave the
    /// engine idle rather than wedged.
    #[test]
    fn the_end_of_the_queue_is_reported_and_leaves_the_engine_idle() {
        // 10x, not the usual 100x — this test counts underruns; see `device_at`.
        let out = device_at(10.0);
        let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
        engine.append(tone(1.0, 0.25), 0).expect("append");

        // Sampled MID-TRACK, not at the end. The tail running out is an
        // underrun by any definition — real ALSA reaches XRun there too, and
        // `drain` uses exactly that to detect end-of-tail — so counting it
        // against us would make this assertion meaningless. What must be zero
        // is a ring that ran dry while the track was still playing.
        assert!(
            wait_for("the track to be well under way", || out.frames_played()
                > out.buffer_frames() as u64 * 2),
            "playback never got going"
        );
        assert_eq!(
            out.underruns(),
            0,
            "the ring ran dry mid-track — on hardware that is an xrun, which is an audible click"
        );

        assert!(
            wait_for("the engine to go empty", || engine.empty()),
            "the engine never reported the end of the queue"
        );
        engine.stop();
    }

    /// A stop must not hang, however the engine was left. The bounded join
    /// exists because an unbounded one once lost the audio thread for the life
    /// of the process.
    #[test]
    fn a_stop_completes_promptly_from_any_state() {
        for (name, pause_first) in [("while playing", false), ("while paused", true)] {
            let out = device();
            let mut engine = PlaybackEngine::new_alsa_direct(out.clone(), false);
            engine.append(tone(5.0, 0.25), 0).expect("append");
            assert!(wait_for("playback to start", || out.frames_played() > 0));
            if pause_first {
                engine.pause();
                std::thread::sleep(Duration::from_millis(10));
            }
            let started = Instant::now();
            engine.stop();
            let took = started.elapsed();
            assert!(
                took < Duration::from_secs(4),
                "stop {name} took {took:?} — the join is not bounded"
            );
        }
    }
}
