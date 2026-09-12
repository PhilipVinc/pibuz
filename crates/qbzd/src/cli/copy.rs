// crates/qbzd/src/cli/copy.rs — normative CLI copy for the auth verbs.
//
// Strings are reproduced verbatim from 02-cli-and-api.md §2.2, "modulo
// interpolated values" (§1.4): the ephemeral listener port is substituted into
// the `ssh -L` forward hint so it is actionable on a headless box, and the
// success line interpolates the validated session's email / plan / user id.

// ============================ error voice (02 §1.4) ============================
// Verbatim §1.4 / §2.2 / §6.3 copy — "modulo interpolated values". Every message
// ends in one to three `→` fix lines; changing the wording is a spec violation.

/// Daemon unreachable — exit 3 (02 §1.4). `host` is the target `ip:port`.
pub fn daemon_down(host: &str) -> String {
    format!(
        "error: daemon not reachable at {host}
  → is it running?    systemctl --user status qbzd
  → just installed?   systemctl --user enable --now qbzd
  → different host?   qbzd --host <ip>:<port> ...  or  export QBZD_HOST=<ip>:<port>"
    )
}

/// Linger-off warning (02 §1.4; NOT an error) — printed by `qbzd status` on the
/// daemon box when `loginctl show-user $USER -p Linger` reports `Linger=no`.
pub fn linger_off(user: &str) -> String {
    format!(
        "warning: linger is off for user '{user}' — the daemon stops when you log out
  → keep it running:  sudo loginctl enable-linger {user}"
    )
}

/// Volume fixed under DSD-direct — exit 5 (02 §1.4, verbatim). Consumed by
/// `error_from_envelope` (cli/client.rs) for the `volume_fixed_dsd` code, so
/// the `volume`/`mute` verbs print this exact block instead of the server's
/// short envelope message.
pub fn volume_fixed_dsd() -> String {
    "error: volume is fixed in DSD-direct mode (bit-perfect passthrough)
  → to get software volume, set DSD mode to \"convert\":  qbzd setup  (Audio screen)"
        .to_string()
}

/// Seek unsupported under DSD-direct — exit 5. No verbatim block is given for
/// seek specifically; 02 §2.2 says it is "the same error-voice family as
/// §1.4 volume copy", so this mirrors `volume_fixed_dsd`'s structure/wording.
/// Consumed by `error_from_envelope` for the `seek_unsupported_dsd` code.
pub fn seek_unsupported_dsd() -> String {
    "error: seek is unsupported in DSD-direct mode (bit-perfect passthrough)
  → to seek, set DSD mode to \"convert\":  qbzd setup  (Audio screen)"
        .to_string()
}

/// Foreign occupant on the control port that is NOT qbzd — printed by the daemon
/// at boot step 5 (02 §2.2, verbatim). `port` is interpolated.
pub fn port_in_use(port: u16) -> String {
    format!(
        "error: port {port} is in use by another process (not qbzd)
  → change the port:  edit [server].port in ~/.config/qbzd/qbzd.toml"
    )
}

/// A DIFFERENT qbzd already answering on the port while our instance lock is on
/// another data root (a stale foreign root). Boot step 5 (02 §8.1-5).
pub fn foreign_qbzd(addr: &str) -> String {
    format!(
        "error: another qbzd is already answering on {addr} (the instance lock said this root is free — stale foreign root?)
  → find it:          ss -ltnp | grep {addr}
  → or change the port:  edit [server].port in ~/.config/qbzd/qbzd.toml"
    )
}

/// LAN-first posture note (FB6, successor to the old 02 §6.3 LAN-exposure
/// warning) — one INFO line logged by the daemon at boot when the control API
/// is NOT loopback-only. Since FB6 the default bind is `0.0.0.0`, so this
/// fires on every default boot; it is deliberately informational, not a
/// `warning:` — an open LAN renderer (Sonos/Chromecast posture) is the
/// intended default, the Origin shield already guards browsers, and this line
/// just orients the operator toward the two ways to restrict it further.
/// `addr` is the bound `ip:port`.
pub fn lan_posture_note(addr: &str) -> String {
    format!(
        "control plane listening on {addr} — anyone on your network can control playback (set [server] bind = \"127.0.0.1\" or [server] token in qbzd.toml to restrict)"
    )
}

/// Version skew — daemon and CLI run different bin semvers (02 §1.6). A warning:
/// `status` still renders. `daemon`/`cli` are the two `version` strings.
pub fn version_skew(daemon: &str, cli: &str) -> String {
    format!(
        "warning: daemon runs {daemon}, this CLI is {cli}
  → restart the daemon:  systemctl --user restart qbzd"
    )
}

/// Breaking api_version skew (02 §1.6) — the verb refuses politely, exit 1.
pub fn api_version_skew(daemon: u32, cli: u32) -> String {
    format!(
        "error: daemon speaks api v{daemon}, this CLI speaks v{cli} — update so both ends run the same package version"
    )
}

// ==================== settings bundle (04-settings-portability.md) ====================
// Verbatim §3 / §4.1 / §5.3 / §5.6 copy — "modulo interpolated values" (§1.4).

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Secret-bearing export warning (04 §3, verbatim). The last two lines teach the
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
next: copy it to the daemon box and run:  qbzd settings import {base} --include-auth"
    )
}

/// Non-secret export success (04 §4.1): the path + the same "next:" hint.
pub fn bundle_export_success(path: &str) -> String {
    let base = basename(path);
    format!(
        "wrote {path} (0600)
next: copy it to the daemon box and run:  qbzd settings import {base}"
    )
}

/// Missing desktop profile on `--from desktop` (04 §4.1, verbatim).
pub fn bundle_no_desktop_profile() -> String {
    "error: no desktop profile found at ~/.local/share/qbz
  is desktop QBZ installed on this box? To export this daemon's settings instead:
  qbzd settings export            (daemon profile is the default)"
        .to_string()
}

/// Version-skew rejection, bundle newer than importer (04 §5.6, verbatim).
pub fn bundle_version_too_new(bundle: i64, supported: i64) -> String {
    format!(
        "error: this bundle is schema v{bundle}; this qbzd understands up to v{supported}
  the bundle came from a newer QBZ — update qbzd on this box, then retry
  (self-built? rebuild from the current tag)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_posture_note_renders_the_verbatim_copy() {
        // FB6 (successor to the old LAN-exposure warning) — one INFO line logged
        // by the daemon at boot when the control API is NOT loopback-only. This
        // test pins the exact wording so it cannot drift from the spec.
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
            rendered.contains("token in qbzd.toml to restrict"),
            "{rendered}"
        );
    }
}
