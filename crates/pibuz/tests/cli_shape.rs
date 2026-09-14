use std::process::Command;

#[test]
fn bare_pibuz_prints_help_and_exits_2() {
    // 01-architecture.md §1.1: a typo'd verb must never leave a daemon running.
    let out = Command::new(env!("CARGO_BIN_EXE_pibuz")).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("Usage"), "help text missing: {text}");
}

#[test]
fn version_answers_locally() {
    // 02-cli-and-api.md §2.2: `pibuz version` needs no daemon, no network.
    let out = Command::new(env!("CARGO_BIN_EXE_pibuz"))
        .arg("version")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("api v1"));
}

/// The keys and values moOde's `startQobuz()` writes on every renderer start.
///
/// moOde configures this daemon by shelling out to `settings set`, once per
/// key, and `sysCmd` discards the exit status — so a key this CLI renames,
/// retires or narrows becomes a setting that silently stops applying on every
/// moOde player, with nothing in the UI to show for it. That has happened
/// three times: a value moOde offered was over the parser's cap, a key was
/// misspelled at the call site, and a negative value could not get past clap
/// at all. Each was invisible until someone read the code.
///
/// So this is a contract, not a convenience: every pair below is what the
/// shipped `www/inc/renderer.php` sends, plus every value its dropdowns in
/// `www/qbz-config.php` can produce. Changing one means changing moOde in the
/// same breath. Deliberately hard-coded rather than read from the moOde tree:
/// this must fail in CI here, where the daemon is built, not only on a box
/// that happens to have both repos checked out.
const MOODE_WRITES: &[(&str, &str)] = &[
    // --- qconnect ---
    ("qconnect.device_name", "Moode Qobuz"),
    ("qconnect.pairing", "on"),
    ("qconnect.volume_mode", "software"),
    ("qconnect.volume_mode", "locked"),
    // `off` is what moOde sends in `locked` mode, and the lowest UI choice.
    ("qconnect.initial_volume", "off"),
    ("qconnect.initial_volume", "10"),
    ("qconnect.initial_volume", "100"),
    // --- playback ---
    ("playback.quality", "mp3"),
    ("playback.quality", "cd"),
    ("playback.quality", "hires"),
    ("playback.quality", "hires_plus"),
    ("playback.persist_session", "false"),
    ("playback.resume_playback_position", "false"),
    ("playback.mpris", "false"),
    // --- audio output: fixed by moOde, never user-visible ---
    ("audio.device", "_audioout"),
    ("audio.device", "btstream"),
    ("audio.backend", "alsa"),
    ("audio.alsa_plugin", "hw"),
    ("audio.alsa_hardware_volume", "false"),
    ("audio.alsa_mixer_device", "auto"),
    // --- audio: every dropdown value ---
    ("audio.stream_buffer_seconds", "2"),
    ("audio.stream_buffer_seconds", "5"),
    ("audio.stream_buffer_seconds", "10"),
    ("audio.normalization_enabled", "true"),
    ("audio.normalization_enabled", "false"),
    // Negative, and the reason `settings set`'s VALUE takes hyphen values.
    ("audio.normalization_target_lufs", "-14"),
    ("audio.normalization_target_lufs", "-18"),
    ("audio.normalization_target_lufs", "-23"),
    ("audio.gapless_enabled", "true"),
    ("audio.gapless_enabled", "false"),
    ("audio.streaming_only", "true"),
    ("audio.streaming_only", "false"),
    ("audio.stream_first_track", "true"),
    ("audio.stream_first_track", "false"),
    // Set from physical memory at boot, not from a dropdown.
    ("audio.cache_to_disk", "true"),
    ("audio.cache_to_disk", "false"),
    ("audio.memory_cache_mb", "auto"),
    ("audio.memory_cache_mb", "512"),
    ("audio.memory_cache_mb", "1024"),
    // Over the old 1024 cap — moOde has offered this the whole time.
    ("audio.memory_cache_mb", "2048"),
    ("audio.disk_cache_mb", "auto"),
    ("audio.disk_cache_mb", "400"),
    ("audio.disk_cache_mb", "2000"),
    ("audio.disk_cache_mb", "8000"),
    ("audio.alsa_buffer_ms", "auto"),
    ("audio.alsa_buffer_ms", "250"),
    ("audio.alsa_buffer_ms", "500"),
    ("audio.alsa_buffer_ms", "1000"),
    ("audio.alsa_buffer_ms", "2000"),
    ("audio.dac_keepalive_ms", "off"),
    ("audio.dac_keepalive_ms", "50"),
    ("audio.dac_keepalive_ms", "100"),
    // --- hooks ---
    ("hooks.script", "/var/local/www/commandw/qbzevent.sh"),
];

#[test]
fn every_setting_moode_writes_is_still_accepted() {
    let dir = std::env::temp_dir().join(format!(
        "pibuz-moode-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch profile dir");

    let mut failures = Vec::new();
    for (key, value) in MOODE_WRITES {
        let out = Command::new(env!("CARGO_BIN_EXE_pibuz"))
            .args(["settings", "set", key, value])
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .output()
            .expect("run pibuz settings set");
        if !out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr);
            failures.push(format!("  {key} = {value}\n    {}", text.trim()));
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "moOde's startQobuz() would fail on {} of {} settings — \
         fix the CLI or update moOde AND this table together:\n{}",
        failures.len(),
        MOODE_WRITES.len(),
        failures.join("\n")
    );
}
