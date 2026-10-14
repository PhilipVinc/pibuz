// crates/pibuz/src/cli/copy.rs — the CLI's fixed copy: the error voice and the
// settings-bundle messages. Verbatim apart from the interpolated values.

// ================================ error voice ================================
// Most messages end in one to three `→` fix lines naming what to do next.

/// Daemon unreachable — exit 3. `host` is the target `ip:port`.
pub fn daemon_down(host: &str) -> String {
    format!(
        "error: daemon not reachable at {host}
  → is it running?    systemctl --user status pibuz
  → just installed?   systemctl --user enable --now pibuz
  → different host?   pibuz --host <ip>:<port> ...  or  export QBZD_HOST=<ip>:<port>"
    )
}

/// Linger-off warning (NOT an error) — printed by `pibuz status` on the
/// daemon box when `loginctl show-user $USER -p Linger` reports `Linger=no`.
pub fn linger_off(user: &str) -> String {
    format!(
        "warning: linger is off for user '{user}' — the daemon stops when you log out
  → keep it running:  sudo loginctl enable-linger {user}"
    )
}

/// Volume fixed under DSD-direct — exit 5. Consumed by
/// `error_from_envelope` (cli/client.rs) for the `volume_fixed_dsd` code, so
/// the `volume`/`mute` verbs print this exact block instead of the server's
/// short envelope message.
pub fn volume_fixed_dsd() -> String {
    "error: volume is fixed in DSD-direct mode (bit-perfect passthrough)
  → to get software volume, set DSD mode to \"convert\":  pibuz setup  (Audio screen)"
        .to_string()
}

/// Seek unsupported under DSD-direct — exit 5. Mirrors `volume_fixed_dsd`'s
/// structure and wording.
/// Consumed by `error_from_envelope` for the `seek_unsupported_dsd` code.
pub fn seek_unsupported_dsd() -> String {
    "error: seek is unsupported in DSD-direct mode (bit-perfect passthrough)
  → to seek, set DSD mode to \"convert\":  pibuz setup  (Audio screen)"
        .to_string()
}

/// Foreign occupant on the control port that is NOT pibuz — printed by the daemon
/// at boot step 5. `port` is interpolated.
pub fn port_in_use(port: u16) -> String {
    format!(
        "error: port {port} is in use by another process (not pibuz)
  → change the port:  edit [server].port in ~/.config/pibuz/pibuz.toml"
    )
}

/// A DIFFERENT pibuz already answering on the port while our instance lock is on
/// another data root (a stale foreign root). Boot step 5.
pub fn foreign_pibuz(addr: &str) -> String {
    format!(
        "error: another pibuz is already answering on {addr} (the instance lock said this root is free — stale foreign root?)
  → find it:          ss -ltnp | grep {addr}
  → or change the port:  edit [server].port in ~/.config/pibuz/pibuz.toml"
    )
}

/// LAN-first posture note — one INFO line logged by the daemon at boot when
/// the control API is NOT loopback-only. The default bind is `0.0.0.0`, so this
/// fires on every default boot; it is deliberately informational, not a
/// `warning:` — an open LAN renderer (Sonos/Chromecast posture) is the
/// intended default, the Origin shield already guards browsers, and this line
/// just orients the operator toward the two ways to restrict it further.
/// `addr` is the bound `ip:port`.
pub fn lan_posture_note(addr: &str) -> String {
    format!(
        "control plane listening on {addr} — anyone on your network can control playback (set [server] bind = \"127.0.0.1\" or [server] token in pibuz.toml to restrict)"
    )
}

/// Version skew — daemon and CLI run different bin semvers. A warning:
/// `status` still renders. `daemon`/`cli` are the two `version` strings.
pub fn version_skew(daemon: &str, cli: &str) -> String {
    format!(
        "warning: daemon runs {daemon}, this CLI is {cli}
  → restart the daemon:  systemctl --user restart pibuz"
    )
}

/// Breaking api_version skew — the verb refuses politely, exit 1.
pub fn api_version_skew(daemon: u32, cli: u32) -> String {
    format!(
        "error: daemon speaks api v{daemon}, this CLI speaks v{cli} — update so both ends run the same package version"
    )
}

// ============================== settings bundle ==============================

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Secret-bearing export warning. The last two lines teach the
/// import step. `path` is the full written path; the basename is interpolated
/// into the import command.
pub fn bundle_secret_warning(path: &str) -> String {
    let base = basename(path);
    format!(
        "WARNING: this bundle contains your Qobuz session token and integration tokens.
Anyone who can read this file can use your accounts.
  - the file was written with mode 0600 — keep it that way
  - move it directly (scp/USB) and delete it after importing
  - it is NOT encrypted: do not mail it, cloud-sync it, or commit it anywhere
wrote {path} (0600, includes secrets)
next: copy it to the daemon box and run:  pibuz settings import {base} --include-auth"
    )
}

/// Non-secret export success: the path + the same "next:" hint.
pub fn bundle_export_success(path: &str) -> String {
    let base = basename(path);
    format!(
        "wrote {path} (0600)
next: copy it to the daemon box and run:  pibuz settings import {base}"
    )
}

/// Version-skew rejection, bundle newer than importer.
pub fn bundle_version_too_new(bundle: i64, supported: i64) -> String {
    format!(
        "error: this bundle is schema v{bundle}; this pibuz understands up to v{supported}
  the bundle came from a newer QBZ — update pibuz on this box, then retry
  (self-built? rebuild from the current tag)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_posture_note_renders_the_verbatim_copy() {
        // One INFO line logged by the daemon at boot when the control API is
        // NOT loopback-only. This test pins the exact wording.
        let rendered = lan_posture_note("0.0.0.0:6789");
        assert!(
            rendered.contains("control plane listening on 0.0.0.0:6789"),
            "{rendered}"
        );
        assert!(
            rendered.contains("anyone on your network can control playback"),
            "{rendered}"
        );
        assert!(rendered.contains("bind = \"127.0.0.1\""), "{rendered}");
        assert!(
            rendered.contains("token in pibuz.toml to restrict"),
            "{rendered}"
        );
    }
}
