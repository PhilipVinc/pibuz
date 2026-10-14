//! qconnect-transport-ws
//!
//! WS transport trait, the native WebSocket transport, and an in-memory
//! transport for tests.

mod config;
mod error;
mod transport;

pub use config::WsTransportConfig;
pub use error::WsTransportError;
pub use transport::{InMemoryWsTransport, NativeWsTransport, TransportEvent, WsTransport};
