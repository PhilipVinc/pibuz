//! The output device, as the playback engine sees it.
//!
//! # Why a trait
//!
//! The engine's decoder and writer threads are where this codebase's most
//! expensive bugs have lived — the position reported after a seek, the gapless
//! hand-off, what a pause does to audio already committed to the device. Every
//! one of them was a contract between components rather than a fault in any one
//! function, so unit-testing the functions individually kept passing while the
//! behaviour was broken.
//!
//! Testing the contract needs a device that is not a DAC. This trait is that
//! seam: [`crate::AlsaDirectStream`] on a Pi, [`VirtualAudioOut`] in a test,
//! and the same engine code driving both.
//!
//! # What it costs
//!
//! One dynamic call per chunk. The writer hands over one or two ALSA periods at
//! a time — four or five calls a second — so the dispatch is not measurable
//! next to the format conversion inside it. This is deliberately NOT a
//! per-sample interface.
//!
//! # What it does not cover
//!
//! The DoP and native-DSD writer keeps the concrete type. Its words are packed
//! by `qbz-dsd` and written verbatim, it shares none of the PCM path's
//! accounting, and nothing is gained by faking a DSD DAC.

use std::sync::atomic::AtomicBool;

/// A PCM output the playback engine can drive.
///
/// Frame counts throughout — never samples, never bytes. A "frame" is one
/// sample per channel, which is the unit ALSA reports and the unit the position
/// accounting is in.
pub trait AudioOut: Send + Sync {
    /// Rate the device is running at, in Hz. Fixed for the life of the stream:
    /// a rate change rebuilds it.
    fn sample_rate(&self) -> u32;

    /// Channel count. Also fixed for the life of the stream.
    fn channels(&self) -> u16;

    /// The hardware ring, in frames, as the driver GRANTED it — not as it was
    /// requested. The writer sizes its work quantum from this.
    fn buffer_frames(&self) -> usize;

    /// One period, in frames, as granted.
    fn period_frames(&self) -> usize;

    /// Hand interleaved `f32` frames over, returning how many FRAMES the device
    /// accepted.
    ///
    /// May accept fewer than offered — a cancelled write returns early — and
    /// the caller must use the returned count rather than assume the whole
    /// buffer landed. `cancel` makes the call interruptible: an implementation
    /// must never block past it indefinitely, because the thread calling this
    /// is the one a stop has to be able to join.
    fn write_f32(&self, samples: &[f32], cancel: &AtomicBool) -> Result<usize, String>;

    /// Top the device up to `target_ms` of queued SILENCE, returning the frames
    /// written. Zero when the device already holds that much, or when silence
    /// would be meaningless (a DSD carrier).
    fn write_silence_to_depth(&self, target_ms: u32, cancel: &AtomicBool) -> Result<usize, String>;

    /// Frames handed over that have not been played yet.
    ///
    /// The heart of the position accounting: what the listener is hearing is
    /// what has been written MINUS this. Zero whenever the number would be
    /// meaningless (the device is not running, or the query failed), which is
    /// the safe direction — the reported position may lag, but never claims
    /// audio was heard that was not.
    fn delay_frames(&self) -> usize;

    /// Start the clock if the device is configured but idle.
    ///
    /// Not optional. The ALSA path sets `start_threshold` to the whole ring, so
    /// a stream that never receives a full ring never starts at all — silently.
    /// Every caller that knows no more audio is coming has to say so.
    fn start_if_prepared(&self) -> Result<(), String>;

    /// Throw away whatever is queued and make the device ready again. The
    /// pause primitive.
    fn discard_queued(&self) -> Result<(), String>;

    /// Let the queued tail play out, then stop. The end-of-track primitive.
    fn drain(&self) -> Result<(), String>;

    /// Stop now, landing on silence. The teardown primitive.
    fn stop(&self) -> Result<(), String>;

    /// Set the device's own volume control, where it has one. `Err` is normal
    /// and not fatal — most USB DACs expose no mixer.
    fn set_hardware_volume(&self, volume: f32) -> Result<(), String>;
}

#[cfg(target_os = "linux")]
impl AudioOut for crate::AlsaDirectStream {
    fn sample_rate(&self) -> u32 {
        crate::AlsaDirectStream::sample_rate(self)
    }
    fn channels(&self) -> u16 {
        crate::AlsaDirectStream::channels(self)
    }
    fn buffer_frames(&self) -> usize {
        crate::AlsaDirectStream::buffer_frames(self)
    }
    fn period_frames(&self) -> usize {
        crate::AlsaDirectStream::period_frames(self)
    }
    fn write_f32(&self, samples: &[f32], cancel: &AtomicBool) -> Result<usize, String> {
        crate::AlsaDirectStream::write_f32(self, samples, cancel)
    }
    fn write_silence_to_depth(&self, target_ms: u32, cancel: &AtomicBool) -> Result<usize, String> {
        crate::AlsaDirectStream::write_silence_to_depth(self, target_ms, cancel)
    }
    fn delay_frames(&self) -> usize {
        crate::AlsaDirectStream::delay_frames(self)
    }
    fn start_if_prepared(&self) -> Result<(), String> {
        crate::AlsaDirectStream::start_if_prepared(self)
    }
    fn discard_queued(&self) -> Result<(), String> {
        crate::AlsaDirectStream::discard_queued(self)
    }
    fn drain(&self) -> Result<(), String> {
        crate::AlsaDirectStream::drain(self)
    }
    fn stop(&self) -> Result<(), String> {
        crate::AlsaDirectStream::stop(self)
    }
    fn set_hardware_volume(&self, volume: f32) -> Result<(), String> {
        crate::AlsaDirectStream::set_hardware_volume(self, volume)
    }
}

/// The same impl for a host that has no ALSA, so the engine still builds for a
/// developer's `cargo check` on macOS. Every method is the stub's.
#[cfg(not(target_os = "linux"))]
impl AudioOut for crate::AlsaDirectStream {
    fn sample_rate(&self) -> u32 {
        crate::AlsaDirectStream::sample_rate(self)
    }
    fn channels(&self) -> u16 {
        crate::AlsaDirectStream::channels(self)
    }
    fn buffer_frames(&self) -> usize {
        crate::AlsaDirectStream::buffer_frames(self)
    }
    fn period_frames(&self) -> usize {
        crate::AlsaDirectStream::period_frames(self)
    }
    fn write_f32(&self, samples: &[f32], cancel: &AtomicBool) -> Result<usize, String> {
        crate::AlsaDirectStream::write_f32(self, samples, cancel)
    }
    fn write_silence_to_depth(&self, target_ms: u32, cancel: &AtomicBool) -> Result<usize, String> {
        crate::AlsaDirectStream::write_silence_to_depth(self, target_ms, cancel)
    }
    fn delay_frames(&self) -> usize {
        crate::AlsaDirectStream::delay_frames(self)
    }
    fn start_if_prepared(&self) -> Result<(), String> {
        crate::AlsaDirectStream::start_if_prepared(self)
    }
    fn discard_queued(&self) -> Result<(), String> {
        crate::AlsaDirectStream::discard_queued(self)
    }
    fn drain(&self) -> Result<(), String> {
        crate::AlsaDirectStream::drain(self)
    }
    fn stop(&self) -> Result<(), String> {
        crate::AlsaDirectStream::stop(self)
    }
    fn set_hardware_volume(&self, volume: f32) -> Result<(), String> {
        crate::AlsaDirectStream::set_hardware_volume(self, volume)
    }
}
