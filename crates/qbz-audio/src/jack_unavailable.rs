//! Stand-in for [`jack_backend`](super::jack_backend) in builds without the
//! `jack` feature.
//!
//! `lib.rs` mounts this file AS `jack_backend` via `#[path]`, so the module
//! path and the `JackStream` name are the same either way and no caller needs
//! a `cfg`. `qbz-player` keeps its `StreamType::Jack` variant, its four
//! dispatch arms and `PlaybackEngine::new_jack`; `pibuz` keeps parsing
//! `backend = "jack"` from a config file. The difference is only that opening
//! the stream now fails with a message instead of dlopening a libjack that a
//! headless build has no reason to carry. See `[features]` in Cargo.toml.
//!
//! Deliberately kept in a SEPARATE file rather than as `#[cfg]` arms inside
//! `jack_backend.rs`: that file is shared with the desktop tree, and this way
//! it needs no edit at all.

use std::convert::Infallible;

/// Uninhabited: [`JackStream::new`] is the only constructor and it always
/// fails, so no value of this type can exist. That is what lets the accessors
/// below discharge their return types with an empty `match` instead of a
/// panic -- there is no runtime path here to get wrong.
pub struct JackStream(Infallible);

impl JackStream {
    /// Always `Err`. The player's dispatch already renders this as
    /// "JACK backend unavailable: {e}" and falls no further.
    pub fn new(_channels: u16) -> Result<Self, String> {
        Err("this build has no JACK support (qbz-audio was compiled without \
             the `jack` feature)"
            .to_string())
    }

    pub fn sample_rate(&self) -> u32 {
        match self.0 {}
    }

    pub fn channels(&self) -> u16 {
        match self.0 {}
    }

    pub fn write_f32(&self, _samples: &[f32]) -> usize {
        match self.0 {}
    }

    pub fn underruns(&self) -> u64 {
        match self.0 {}
    }
}
