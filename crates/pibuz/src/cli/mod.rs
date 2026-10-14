// crates/pibuz/src/cli/mod.rs — human-facing CLI presentation.
//
// The CLI is a stateless renderer of what the daemon's API returns; its error
// voice and shared copy strings live in one place, `copy`.
pub mod client;
pub(crate) mod copy;
pub(crate) mod mode;
pub(crate) mod play;
pub(crate) mod queue;
pub mod service;
pub mod settings;
pub(crate) mod status;
pub(crate) mod transport;
pub(crate) mod watch;
