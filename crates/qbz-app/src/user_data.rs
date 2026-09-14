//! The desktop install's `last_user_id` marker.
//!
//! All that survives of the per-user profile layout: `pibuz settings export
//! --from desktop` reads this marker to decide whose per-user domains to pull
//! out of a desktop install. The daemon itself is single-profile and stores
//! everything under its own root, so nothing here is per-user any more.

use std::path::PathBuf;

/// The desktop (non-user-scoped) data directory: `<data_dir>/qbz`.
pub fn global_data_dir() -> Result<PathBuf, String> {
    dirs::data_dir()
        .ok_or_else(|| "Could not determine data directory".to_string())
        .map(|d| d.join("qbz"))
}

/// Read the desktop install's last active user id. `None` when the marker is
/// missing or unparseable.
pub fn load_last_user_id() -> Option<u64> {
    let contents = std::fs::read_to_string(global_data_dir().ok()?.join("last_user_id")).ok()?;
    contents.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_data_dir_is_scoped_to_qbz() {
        assert!(global_data_dir().expect("global data dir").ends_with("qbz"));
    }
}
