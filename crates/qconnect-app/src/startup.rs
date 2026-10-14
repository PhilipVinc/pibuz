//! QConnect startup mode: persisted preference for whether QConnect
//! auto-connects when the daemon starts.
//!
//! Pure logic only — no I/O. Persistence lives in the host, next to
//! its `device_name` handling (`pibuz::qconnect::transport`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QconnectStartupMode {
    /// Never auto-connect on startup. Default for fresh installs.
    #[default]
    Off,
    /// Always auto-connect on every startup, regardless of last state.
    On,
    /// Restore the on/off state captured at last connect/disconnect.
    RememberLast,
}

impl QconnectStartupMode {
    pub fn as_str(self) -> &'static str {
        match self {
            QconnectStartupMode::Off => "off",
            QconnectStartupMode::On => "on",
            QconnectStartupMode::RememberLast => "remember_last",
        }
    }

    // Not `FromStr`: this is total (or Option-returning) with no error
    // type to report, and the trait would force a Result.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "off" => Some(QconnectStartupMode::Off),
            "on" => Some(QconnectStartupMode::On),
            "remember_last" => Some(QconnectStartupMode::RememberLast),
            _ => None,
        }
    }

    /// Whether QConnect should auto-connect at startup in this mode.
    ///
    /// `RememberLast` is still accepted, because a stored value has to keep
    /// parsing, but nothing records the last state, so it resolves to off.
    pub fn auto_connects(self) -> bool {
        match self {
            QconnectStartupMode::On => true,
            QconnectStartupMode::Off | QconnectStartupMode::RememberLast => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off() {
        assert_eq!(QconnectStartupMode::default(), QconnectStartupMode::Off);
    }

    #[test]
    fn round_trip_str() {
        for m in [
            QconnectStartupMode::Off,
            QconnectStartupMode::On,
            QconnectStartupMode::RememberLast,
        ] {
            assert_eq!(QconnectStartupMode::from_str(m.as_str()), Some(m));
        }
    }

    #[test]
    fn from_str_unknown_value_returns_none() {
        assert_eq!(QconnectStartupMode::from_str("auto"), None);
        assert_eq!(QconnectStartupMode::from_str(""), None);
    }

    #[test]
    fn only_on_auto_connects() {
        assert!(QconnectStartupMode::On.auto_connects());
        assert!(!QconnectStartupMode::Off.auto_connects());
        assert!(!QconnectStartupMode::RememberLast.auto_connects());
    }
}
