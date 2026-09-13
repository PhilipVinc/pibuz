//! D-Bus device reservation via org.freedesktop.ReserveDevice1.
//!
//! Acquires the bus name org.freedesktop.ReserveDevice1.Audio<N> for the
//! card index N, signalling to PulseAudio/PipeWire/WirePlumber that another
//! application owns the device exclusively. Released on Drop.
//!
//! The protocol and the lifetime model are documented on the Linux
//! implementation in `device_reservation/linux.rs`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod stub;
#[cfg(not(target_os = "linux"))]
pub use stub::*;
