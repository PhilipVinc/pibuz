// crates/pibuz/src/cli/mod.rs — human-facing CLI presentation.
//
// The CLI is a stateless renderer of what the daemon's API returns; its error
// voice and shared copy strings live in one place, `copy`.
pub mod client;
pub mod copy;
pub mod mode;
pub mod play;
pub mod queue;
pub mod service;
pub mod settings;
pub mod status;
pub mod transport;
pub mod watch;
