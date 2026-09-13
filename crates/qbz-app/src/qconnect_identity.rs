//! Persistent QConnect device identity.
//!
//! The QConnect device UUID must be stable across runs. It is persisted in a
//! small SQLite settings database (key `device_uuid`) whose path the CALLER
//! supplies — `qbzd` points it at the daemon root so the daemon keeps its own
//! identity. See `qbzd::qconnect::transport::resolve_qconnect_device_uuid`,
//! which owns the path resolution and the `QBZ_QCONNECT_DEVICE_UUID` override.

use uuid::Uuid;

/// Load the persisted device_uuid from `path`, generating + persisting one on
/// first run. Split out so the persistence round-trip is unit-testable against a
/// temp path.
pub fn device_uuid_from_db(path: &std::path::Path) -> String {
    if let Some(existing) = load_persisted_device_uuid(path) {
        return existing;
    }
    let generated = Uuid::new_v4().to_string();
    persist_device_uuid(path, &generated);
    generated
}

/// Load the persisted device_uuid. Returns None if not set or on any error.
fn load_persisted_device_uuid(path: &std::path::Path) -> Option<String> {
    let conn = rusqlite::Connection::open(path).ok()?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .ok()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )
    .ok()?;
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'device_uuid'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|v| !v.trim().is_empty())
}

/// Persist the device_uuid to disk (INSERT OR REPLACE).
fn persist_device_uuid(path: &std::path::Path, uuid: &str) {
    let Ok(conn) = rusqlite::Connection::open(path) else {
        return;
    };
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;");
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    );
    let _ = conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('device_uuid', ?1)",
        rusqlite::params![uuid],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_uuid_persists_and_is_reused_across_calls() {
        let tmp =
            std::env::temp_dir().join(format!("qbz_qconnect_uuid_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        // First call generates and persists.
        let first = device_uuid_from_db(&tmp);
        assert!(!first.trim().is_empty(), "generated uuid must be non-empty");
        // Second call must return the SAME value (read back from disk, not a fresh v4).
        let second = device_uuid_from_db(&tmp);
        assert_eq!(first, second, "device_uuid must be stable across calls");

        let _ = std::fs::remove_file(&tmp);
    }
}
