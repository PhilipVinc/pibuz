// crates/pibuz/src/cli/status.rs — the `status` and `ping` verbs (02 §2.2).
//
// Both render an already-parsed API payload; neither holds state. `status` also
// runs the version-skew check (§1.6, from the /api/status payload — it carries
// `version` + `api_version`, so it needs no /api/info fallback) and, on the
// daemon box, the linger check (§1.4). Exit codes come from the frozen table
// (§1.3): 0 healthy · 3 unreachable · 5 device unopenable.
use serde_json::Value;

use crate::cli::client::ApiClient;
use crate::cli::copy;
use crate::paths::ProfileRoots;

/// `pibuz ping` — liveness. Human `pong`; `--json` the raw body. Exit 0 · 3.
pub async fn ping(host: Option<String>, json: bool, roots: &ProfileRoots) -> i32 {
    let client = ApiClient::new(host, roots);
    match client.get("/api/ping").await {
        Ok(v) => {
            if json {
                println!("{}", serde_json::to_string(&v).unwrap_or_default());
            } else {
                println!("pong");
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            e.exit_code()
        }
    }
}

/// `pibuz status` — THE diagnostic. Human composite block; `--json` raw payload.
/// `verbose` (`-v`) adds the memory, cache and buffer sections; `--json` is the
/// whole payload either way, so a script never has to pass it.
/// Exit 0 healthy · 3 unreachable · 5 device unopenable.
pub async fn status(host: Option<String>, json: bool, verbose: bool, roots: &ProfileRoots) -> i32 {
    let client = ApiClient::new(host, roots);
    let payload = match client.get("/api/status").await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return e.exit_code();
        }
    };

    // Version skew (§1.6): breaking api_version mismatch refuses; a semver-only
    // mismatch is a warning that does not stop the render.
    let daemon_api = payload
        .get("api_version")
        .and_then(|a| a.as_u64())
        .unwrap_or(0) as u32;
    if daemon_api != crate::API_VERSION {
        eprintln!("{}", copy::api_version_skew(daemon_api, crate::API_VERSION));
        return 1;
    }
    // `crate::VERSION`, NOT `CARGO_PKG_VERSION`: the daemon reports the
    // former (a local Pi build stamps `PIBUZ_BUILD_ID`,
    // and a local Pi build stamps a `2.1.0.local-<sha>`). Comparing against
    // the bare Cargo version made every stamped build warn that it was skewed
    // against ITSELF — same binary, same process even, two different strings.
    let cli_ver = crate::VERSION;
    if let Some(daemon_ver) = payload.get("version").and_then(|v| v.as_str()) {
        if !daemon_ver.is_empty() && daemon_ver != cli_ver {
            eprintln!("{}", copy::version_skew(daemon_ver, cli_ver));
        }
    }

    if json {
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        print!("{}", render(&payload, client.host(), verbose));
    }

    // Linger check on the daemon box only (§1.4) — a warning, never fatal.
    if client.is_local() {
        if let Some(w) = linger_warning() {
            eprintln!("{w}");
        }
    }

    exit_from_state(&payload)
}

/// 5 configured device not present · else 0.
///
/// Exit 4 (`needs_auth`) is gone with the account path. It fired whenever the
/// daemon had no Qobuz login — which, for a renderer that gets its credentials
/// from a Connect handoff, is ALWAYS. A healthy Pi sitting ready to be cast to
/// was reporting itself as failed to every script and monitor that checked it.
fn exit_from_state(p: &Value) -> i32 {
    let configured = p
        .pointer("/audio/configured_device")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let present = p
        .pointer("/audio/device_present")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if configured && !present {
        return 5;
    }
    0
}

/// The composite block. `host` is the target (the payload has no `bind`);
/// `verbose` adds the memory and buffer sections and the per-track streaming
/// rows — the figures you want when something sounds wrong, and clutter when
/// you are only asking whether the daemon is up.
///
/// Every row is `label: value` in a fixed column so the block scans vertically,
/// and no line is wider than ~72 columns. A row whose value the daemon does not
/// know is OMITTED rather than printed empty: an idle daemon is a short block,
/// and `output` appears only once a stream has existed to measure it.
fn render(p: &Value, host: &str, verbose: bool) -> String {
    let mut b = Block::default();

    let version = str_at(p, &["version"]);
    let api = p.get("api_version").and_then(|a| a.as_u64()).unwrap_or(0);
    let uptime = fmt_uptime(p.get("uptime_secs").and_then(|u| u.as_u64()).unwrap_or(0));
    b.line(format!("Pibuz {version} · api v{api} · up {uptime}"));

    let online = p
        .pointer("/network/online")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    b.row(
        "host",
        format!("{host} · {}", if online { "online" } else { "OFFLINE" }),
    );
    let data_root = str_at(p, &["data_root"]);
    if !data_root.is_empty() {
        b.row("data", data_root);
    }
    if verbose {
        if let Some(ms) = p.get("driver_tick_age_ms").and_then(|v| v.as_u64()) {
            b.row("tick", format!("{ms} ms ago"));
        }
    }
    match last_error(p) {
        Some(e) => b.row("error", e),
        None if verbose => b.row("errors", "none".into()),
        None => {}
    }

    audio_section(&mut b, p);
    playback_section(&mut b, p, verbose);
    if verbose {
        memory_section(&mut b, p);
        buffers_section(&mut b, p);
    }
    qconnect_section(&mut b, p);

    b.finish()
}

/// Accumulates the sectioned block. `row` pads the label into a fixed column;
/// `cont` continues the previous row's value under that column (the L2 cache
/// directory, which is the one value that will not fit beside its label).
#[derive(Default)]
struct Block {
    out: String,
}

/// Indent + label column width. Values start at column 12.
const LABEL: usize = 9;

impl Block {
    fn line(&mut self, s: String) {
        self.out.push_str(&s);
        self.out.push('\n');
    }

    fn section(&mut self, name: &str) {
        self.out.push('\n');
        self.line(name.to_string());
    }

    fn row(&mut self, label: &str, value: String) {
        self.line(format!("  {label:<LABEL$} {value}"));
    }

    fn cont(&mut self, value: String) {
        self.line(format!("  {:<LABEL$} {value}", ""));
    }

    fn finish(self) -> String {
        self.out
    }
}

fn audio_section(b: &mut Block, p: &Value) {
    b.section("audio");

    let backend = p.pointer("/audio/backend").and_then(|v| v.as_str());
    let device = p
        .pointer("/audio/configured_device")
        .and_then(|v| v.as_str());
    let mut dev = match (device, backend) {
        (Some(d), Some(be)) => format!("{d} ({be})"),
        (Some(d), None) => d.to_string(),
        (None, Some(be)) => format!("system default ({be})"),
        (None, None) => "system default".to_string(),
    };
    if p.pointer("/audio/device_present")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        dev.push_str(" · present");
    } else {
        dev.push_str(" · NOT PRESENT");
    }
    if p.pointer("/audio/device_open")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        dev.push_str(" · open");
    }
    b.row("device", dev);

    // A named PCM is opened directly, but whatever its chain does next is
    // invisible from here — with CamillaDSP or an equalizer behind the name the
    // stream is converted before it reaches the DAC. Say so rather than
    // printing a claim the daemon cannot stand behind.
    if let Some(mode) = p.pointer("/audio/bit_perfect").and_then(|v| v.as_str()) {
        b.row(
            "mode",
            match mode {
                "DirectHardware" => "bit-perfect, direct to the hardware".into(),
                "DirectNamedDevice" => "direct to the named device (its chain decides)".into(),
                "PluginFallback" => "plughw plugin · ALSA may convert".into(),
                "Disabled" => "shared system path · not bit-perfect".into(),
                other => other.to_string(),
            },
        );
    }

    // Two rates, and the interesting case is when they disagree: the stream is
    // what the file holds, the output is what the device actually runs at, and
    // anything in between (a shared PipeWire/Pulse/CPAL path, an ALSA config
    // pinning a rate) resamples silently.
    let sr = p.pointer("/audio/sample_rate").and_then(|v| v.as_u64());
    let bd = p.pointer("/audio/bit_depth").and_then(|v| v.as_u64());
    let out_sr = p
        .pointer("/audio/output_sample_rate")
        .and_then(|v| v.as_u64());
    match (sr, bd) {
        (Some(sr), Some(bd)) => b.row("stream", format!("{} / {bd}-bit", fmt_hz(sr))),
        (Some(sr), None) => b.row("stream", fmt_hz(sr)),
        _ => {}
    }
    if let Some(out) = out_sr {
        b.row(
            "output",
            match sr {
                Some(sr) if sr != out => {
                    format!("{} · RESAMPLED from {}", fmt_hz(out), fmt_hz(sr))
                }
                _ => fmt_hz(out),
            },
        );
    }
}

fn playback_section(b: &mut Block, p: &Value, verbose: bool) {
    b.section("playback");

    let state = str_at(p, &["playback", "state"]);
    let queue = p
        .pointer("/playback/queue_len")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let muted = p
        .pointer("/playback/muted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let level = if muted {
        "muted".to_string()
    } else {
        let vol = p
            .pointer("/playback/volume")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        format!("vol {}%", (vol * 100.0).round() as i64)
    };
    b.row("state", format!("{state} · {level} · queue {queue}"));

    if state == "stopped" {
        return;
    }

    let title = p.pointer("/playback/title").and_then(|v| v.as_str());
    let artist = p.pointer("/playback/artist").and_then(|v| v.as_str());
    b.row(
        "track",
        match (title, artist) {
            (Some(t), Some(a)) => format!("{t} — {a}"),
            (Some(t), None) => t.to_string(),
            _ => "(unknown track)".to_string(),
        },
    );
    let pos = p
        .pointer("/playback/position")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let dur = p
        .pointer("/playback/duration")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    b.row("time", format!("{} / {}", fmt_mmss(pos), fmt_mmss(dur)));

    if !verbose {
        return;
    }
    if let Some(prog) = p
        .pointer("/playback/buffer_progress")
        .and_then(|v| v.as_f64())
    {
        b.row("buffer", format!("{}% downloaded", (prog * 100.0).round()));
    }
    if p.pointer("/playback/gapless_ready")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        b.row(
            "gapless",
            match p
                .pointer("/playback/gapless_next_track_id")
                .and_then(|v| v.as_u64())
            {
                Some(id) => format!("next track armed (#{id})"),
                None => "next track armed".to_string(),
            },
        );
    }
    if let Some(gain) = p
        .pointer("/playback/normalization_gain")
        .and_then(|v| v.as_f64())
    {
        if gain > 0.0 {
            b.row(
                "gain",
                format!("{:+.1} dB (normalized)", 20.0 * gain.log10()),
            );
        }
    }
}

fn memory_section(b: &mut Block, p: &Value) {
    let Some(cache) = p.get("cache") else {
        return; // older daemon: no section rather than a section of zeros
    };
    b.section("memory");

    if let Some(mem) = p.get("memory") {
        let mut host = String::new();
        if let Some(kb) = mem.get("total_kb").and_then(|v| v.as_u64()) {
            host.push_str(&format!("{} RAM · ", fmt_bytes(kb.saturating_mul(1024))));
        }
        host.push_str(&format!(
            "{} profile",
            mem.get("class").and_then(|v| v.as_str()).unwrap_or("?")
        ));
        if mem.get("gapless_prefetch").and_then(|v| v.as_bool()) == Some(false) {
            host.push_str(" · no gapless");
        }
        b.row("host", host);
    }

    if let Some(l1) = cache.get("l1") {
        let mut row = format!(
            "{} · {} of {}",
            fmt_tracks(l1.get("tracks").and_then(|v| v.as_u64()).unwrap_or(0)),
            fmt_bytes(l1.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0)),
            fmt_bytes(l1.get("budget_bytes").and_then(|v| v.as_u64()).unwrap_or(0)),
        );
        match l1.get("fetching").and_then(|v| v.as_u64()).unwrap_or(0) {
            0 => {}
            n => row.push_str(&format!(" · {n} fetching")),
        }
        b.row("L1 audio", row);
    }

    match cache.get("l2") {
        Some(l2) if !l2.is_null() => {
            b.row(
                "L2 disk",
                format!(
                    "{} · {} of {}",
                    fmt_tracks(l2.get("tracks").and_then(|v| v.as_u64()).unwrap_or(0)),
                    fmt_bytes(l2.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0)),
                    fmt_bytes(l2.get("budget_bytes").and_then(|v| v.as_u64()).unwrap_or(0)),
                ),
            );
            if let Some(dir) = l2.get("dir").and_then(|v| v.as_str()) {
                b.cont(dir.to_string());
            }
        }
        // Not a failure worth an error row: `audio.cache_to_disk` off is a
        // deliberate setting on a box whose card you do not want written to.
        Some(_) => b.row("L2 disk", "off · nothing is written to the card".into()),
        None => {}
    }
}

fn buffers_section(b: &mut Block, p: &Value) {
    let Some(bufs) = p.get("buffers") else {
        return;
    };
    b.section("buffers");

    if let Some(secs) = bufs.get("window_seconds").and_then(|v| v.as_u64()) {
        b.row(
            "window",
            format!(
                "{secs} s compressed · {} max",
                fmt_bytes(
                    bufs.get("window_max_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                )
            ),
        );
    }
    if let Some(bytes) = bufs.get("initial_max_bytes").and_then(|v| v.as_u64()) {
        b.row("initial", format!("{} max before start", fmt_bytes(bytes)));
    }
    if let Some(ms) = bufs.get("pcm_ring_ms").and_then(|v| v.as_u64()) {
        // A null `alsa_buffer_ms` is the rate-derived default, which depends on
        // the stream and so has no figure here — but "not pinned" is itself
        // what a reader chasing a dropout wants to know.
        let alsa = match bufs.get("alsa_buffer_ms").and_then(|v| v.as_u64()) {
            Some(ms) => fmt_ms(ms),
            None => "auto".to_string(),
        };
        b.row("ring", format!("{} decoded · ALSA {alsa}", fmt_ms(ms)));
    }
}

fn qconnect_section(b: &mut Block, p: &Value) {
    b.section("qconnect");

    if !p
        .pointer("/qconnect/enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        b.row("state", "off".into());
        return;
    }

    let mut state = p
        .pointer("/qconnect/state")
        .and_then(|v| v.as_str())
        .unwrap_or("enabled")
        .to_string();
    // `is_active` is the one a controller changes without touching anything
    // else: switching to its own speakers leaves us connected and no longer
    // rendering, which looks identical on every other field.
    match p.pointer("/qconnect/is_active").and_then(|v| v.as_bool()) {
        Some(true) => state.push_str(" · active renderer"),
        Some(false)
            if p.pointer("/qconnect/session_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false) =>
        {
            state.push_str(" · session up, NOT rendering")
        }
        _ => {}
    }
    b.row("state", state);

    if let Some(name) = p.pointer("/qconnect/device_name").and_then(|v| v.as_str()) {
        if !name.is_empty() {
            b.row("name", name.to_string());
        }
    }
    if p.pointer("/qconnect/pairing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        b.row(
            "pairing",
            match p.pointer("/qconnect/pairing_port").and_then(|v| v.as_u64()) {
                Some(port) => format!("serving on :{port}"),
                None => "serving".to_string(),
            },
        );
    }
}

/// The first latched error, in the order they matter for diagnosis.
fn last_error(p: &Value) -> Option<String> {
    for key in ["stream", "auth", "transport"] {
        if let Some(m) = p
            .pointer(&format!("/last_errors/{key}"))
            .and_then(|v| v.as_str())
        {
            if !m.is_empty() {
                return Some(format!("{key}: {m}"));
            }
        }
    }
    None
}

/// `loginctl show-user $USER -p Linger` → the §1.4 linger warning on `Linger=no`.
/// Any failure (no loginctl, no session) → no warning.
fn linger_warning() -> Option<String> {
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty())?;
    let out = std::process::Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim() == "Linger=no" {
        Some(copy::linger_off(&user))
    } else {
        None
    }
}

fn str_at(p: &Value, path: &[&str]) -> String {
    let mut cur = p;
    for k in path {
        match cur.get(k) {
            Some(v) => cur = v,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or("").to_string()
}

/// `44100` → `44.1 kHz`, `192000` → `192 kHz`. A rate is easier to compare
/// against another rate at this scale than as six raw digits.
fn fmt_hz(hz: u64) -> String {
    if hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", hz as f64 / 1000.0)
    }
}

/// Powers of two, labelled `KB`/`MB`/`GB` — the same arithmetic and the same
/// labels the daemon's own log lines use (`l1_max_bytes / (1024 * 1024)` is
/// logged as MB), so a figure here compares directly against one in the log.
fn fmt_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.0} KB", b / K)
    } else if b < K * K * K {
        format!("{:.0} MB", b / (K * K))
    } else {
        format!("{:.1} GB", b / (K * K * K))
    }
}

/// Sub-second depths stay in ms, where they were configured; anything longer
/// reads as seconds, where a stall budget is easier to think about.
fn fmt_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

fn fmt_tracks(n: u64) -> String {
    if n == 1 {
        "1 track".to_string()
    } else {
        format!("{n} tracks")
    }
}

fn fmt_mmss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn fmt_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_payload() -> Value {
        serde_json::json!({
            "version": "2.4.0", "api_version": 1, "uptime_secs": 259_200,
            "data_root": "/home/pi/.local/share/pibuz", "driver_tick_age_ms": 210,
            "audio": {"backend": "alsa", "configured_device": "hw:CARD=D30,DEV=0",
                      "device_present": true, "device_open": true,
                      "bit_perfect": "DirectHardware", "sample_rate": 192000,
                      "bit_depth": 24, "output_sample_rate": 192000},
            "playback": {"state": "playing", "track_id": 176544871, "title": "Spain",
                         "artist": "Chick Corea", "position": 192, "duration": 581,
                         "volume": 0.8, "muted": false, "queue_len": 14,
                         "buffer_progress": 0.98, "gapless_ready": true,
                         "gapless_next_track_id": 176544872, "normalization_gain": null},
            "qconnect": {"enabled": true, "state": "connected", "device_name": "Pibuz (kitchen-pi)",
                         "session_active": true, "is_active": true,
                         "last_transport_reconnect": null, "pairing": true, "pairing_port": 9100},
            "network": {"online": true},
            "memory": {"class": "normal", "total_kb": 3_998_000,
                       "gapless_prefetch": true},
            "cache": {"l1": {"tracks": 2, "bytes": 192_937_984,
                             "budget_bytes": 675_282_944, "fetching": 1},
                      "l2": {"tracks": 37, "bytes": 1_288_490_188,
                             "budget_bytes": 838_860_800,
                             "dir": "/home/pi/.cache/pibuz/audio"}},
            "buffers": {"window_seconds": 8, "window_max_bytes": 33_554_432,
                        "initial_max_bytes": 2_097_152, "pcm_ring_ms": 6000,
                        "alsa_buffer_ms": null},
            "last_errors": {"stream": null, "auth": null, "transport": null}
        })
    }

    #[test]
    fn healthy_status_exits_zero() {
        assert_eq!(exit_from_state(&healthy_payload()), 0);
    }

    #[test]
    fn a_renderer_that_has_not_been_cast_to_is_still_healthy() {
        // The regression this replaced: `exit_from_state` returned 4 whenever
        // the daemon had no Qobuz account. A renderer gets its credentials
        // from a Connect handoff, so that was every healthy Pi, every time —
        // `pibuz status` reported failure to every script that checked it.
        let p = healthy_payload();
        assert!(
            p.get("auth").is_none(),
            "there is no account state any more"
        );
        assert_eq!(exit_from_state(&p), 0);
    }

    #[test]
    fn configured_but_absent_device_exits_five() {
        let mut p = healthy_payload();
        p["audio"]["device_present"] = serde_json::json!(false);
        assert_eq!(exit_from_state(&p), 5);
        // system default (no configured device) never trips exit 5.
        let mut sysdef = healthy_payload();
        sysdef["audio"]["configured_device"] = serde_json::Value::Null;
        sysdef["audio"]["device_present"] = serde_json::json!(false);
        assert_eq!(exit_from_state(&sysdef), 0);
    }

    /// Every row of a full verbose block, against real Pi figures. The widest
    /// line here is the one a narrow terminal wraps, which is the thing the
    /// sectioned layout exists to fix — so it is asserted, not eyeballed.
    #[test]
    fn no_line_is_wider_than_the_terminal_it_is_read_in() {
        let block = render(&healthy_payload(), "192.168.1.40:8182", true);
        for line in block.lines() {
            assert!(
                line.chars().count() <= 72,
                "{} columns: {line}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn the_default_block_covers_the_health_question() {
        let block = render(&healthy_payload(), "192.168.1.40:8182", false);
        assert!(block.contains("Pibuz 2.4.0 · api v1 · up 3d 0h"), "{block}");
        assert!(
            block.contains("host      192.168.1.40:8182 · online"),
            "{block}"
        );
        assert!(
            block.contains("device    hw:CARD=D30,DEV=0 (alsa) · present · open"),
            "{block}"
        );
        assert!(
            block.contains("mode      bit-perfect, direct to the hardware"),
            "{block}"
        );
        assert!(block.contains("stream    192 kHz / 24-bit"), "{block}");
        assert!(block.contains("output    192 kHz"), "{block}");
        assert!(
            block.contains("state     playing · vol 80% · queue 14"),
            "{block}"
        );
        assert!(block.contains("track     Spain — Chick Corea"), "{block}");
        assert!(block.contains("time      3:12 / 9:41"), "{block}");
        assert!(
            !block.contains("auth"),
            "the block must not carry an account line — there is no account: {block}"
        );
    }

    /// The whole point of the split: the figures you want when something sounds
    /// wrong stay out of the block you read to ask whether the daemon is up.
    #[test]
    fn the_memory_and_buffer_sections_are_verbose_only() {
        let plain = render(&healthy_payload(), "127.0.0.1:8182", false);
        // Row labels, not bare words: `ring` is a substring of `pairing`, which
        // the default block legitimately carries.
        for absent in [
            "\nmemory\n",
            "\nbuffers\n",
            "  L1 audio ",
            "  L2 disk ",
            "  ring ",
            "  tick ",
            "  buffer ",
            "  gapless ",
        ] {
            assert!(
                !plain.contains(absent),
                "{absent:?} leaked into the default block:\n{plain}"
            );
        }

        let verbose = render(&healthy_payload(), "127.0.0.1:8182", true);
        assert!(
            verbose.contains("host      3.8 GB RAM · normal profile"),
            "{verbose}"
        );
        assert!(
            verbose.contains("L1 audio  2 tracks · 184 MB of 644 MB · 1 fetching"),
            "{verbose}"
        );
        assert!(
            verbose.contains("L2 disk   37 tracks · 1.2 GB of 800 MB"),
            "{verbose}"
        );
        assert!(verbose.contains("/home/pi/.cache/pibuz/audio"), "{verbose}");
        assert!(
            verbose.contains("window    8 s compressed · 32 MB max"),
            "{verbose}"
        );
        assert!(
            verbose.contains("initial   2 MB max before start"),
            "{verbose}"
        );
        assert!(verbose.contains("ring      6.0 s decoded"), "{verbose}");
        assert!(verbose.contains("buffer    98% downloaded"), "{verbose}");
        assert!(
            verbose.contains("gapless   next track armed (#176544872)"),
            "{verbose}"
        );
        assert!(verbose.contains("tick      210 ms ago"), "{verbose}");
    }

    /// A row the daemon has no answer for is absent, not blank. An idle daemon
    /// is the case that proves it: no track, no rates, no stream.
    #[test]
    fn an_idle_daemon_renders_a_short_block() {
        let mut p = healthy_payload();
        p["playback"] = serde_json::json!({"state": "stopped", "volume": 0.8,
                                           "muted": false, "queue_len": 14});
        p["audio"]["sample_rate"] = Value::Null;
        p["audio"]["bit_depth"] = Value::Null;
        p["audio"]["output_sample_rate"] = Value::Null;
        p["audio"]["bit_perfect"] = Value::Null;
        p["audio"]["device_open"] = serde_json::json!(false);

        let block = render(&p, "127.0.0.1:8182", false);
        assert!(
            block.contains("state     stopped · vol 80% · queue 14"),
            "{block}"
        );
        for absent in ["track", "time", "stream", "output", "mode", "open"] {
            assert!(
                !block.contains(absent),
                "{absent} rendered with nothing to say:\n{block}"
            );
        }
    }

    /// `pibuz status` is the first thing anyone runs when a box has stopped
    /// making noise, so the two failures that explain it must be impossible to
    /// scroll past: the device is gone, or a controller took the session away.
    #[test]
    fn the_failures_worth_running_status_for_shout() {
        let mut p = healthy_payload();
        p["audio"]["device_present"] = serde_json::json!(false);
        p["audio"]["output_sample_rate"] = serde_json::json!(44100);
        p["network"]["online"] = serde_json::json!(false);
        p["qconnect"]["is_active"] = serde_json::json!(false);
        p["last_errors"]["stream"] = serde_json::json!("404 from the CDN");

        let block = render(&p, "127.0.0.1:8182", false);
        assert!(block.contains("NOT PRESENT"), "{block}");
        assert!(block.contains("OFFLINE"), "{block}");
        assert!(
            block.contains("output    44.1 kHz · RESAMPLED from 192 kHz"),
            "{block}"
        );
        assert!(
            block.contains("state     connected · session up, NOT rendering"),
            "{block}"
        );
        assert!(
            block.contains("error     stream: 404 from the CDN"),
            "{block}"
        );
    }

    /// An older daemon (or one with the disk cache off) has no `cache` object
    /// at all. A section of zeros would read as "the cache is empty", which is
    /// a different and wrong fact — so there is no section.
    #[test]
    fn a_payload_without_the_new_sections_omits_them() {
        let mut p = healthy_payload();
        p.as_object_mut().unwrap().remove("cache");
        p.as_object_mut().unwrap().remove("buffers");
        let block = render(&p, "127.0.0.1:8182", true);
        assert!(!block.contains("memory"), "{block}");
        assert!(!block.contains("buffers"), "{block}");
        // …and the rest of the block still renders.
        assert!(block.contains("track     Spain — Chick Corea"), "{block}");
    }

    /// A daemon on a box with no `/proc/meminfo` sends `total_kb: null`. The
    /// class is still worth a row; a RAM figure is not invented to go with it.
    #[test]
    fn a_missing_ram_figure_leaves_the_class_standing() {
        let mut p = healthy_payload();
        p["memory"]["total_kb"] = Value::Null;
        let block = render(&p, "127.0.0.1:8182", true);
        assert!(block.contains("host      normal profile"), "{block}");
        assert!(!block.contains("RAM"), "{block}");
    }

    /// The host row claims only what the daemon enforces. A 1 GB Pi is
    /// LowMemory and prefetches at whatever quality the listener set — nothing
    /// downgrades Hi-Res anywhere — so the row said "no Hi-Res prefetch" to
    /// every owner of one for as long as it was rendered from a flag no code
    /// read. The class and the gapless answer are the two facts left.
    #[test]
    fn a_low_memory_host_is_not_told_it_lost_hi_res() {
        let mut p = healthy_payload();
        p["memory"]["class"] = Value::from("low");
        p["memory"]["total_kb"] = Value::from(926_832);
        let block = render(&p, "127.0.0.1:8182", true);
        assert!(
            block.contains("host      905 MB RAM · low profile"),
            "{block}"
        );
        assert!(!block.contains("Hi-Res"), "{block}");
        assert!(!block.contains("no gapless"), "{block}");
    }

    #[test]
    fn a_disk_cache_that_is_off_says_so() {
        let mut p = healthy_payload();
        p["cache"]["l2"] = Value::Null;
        let block = render(&p, "127.0.0.1:8182", true);
        assert!(
            block.contains("L2 disk   off · nothing is written to the card"),
            "{block}"
        );
    }

    #[test]
    fn qconnect_that_is_off_is_one_row() {
        let mut p = healthy_payload();
        p["qconnect"]["enabled"] = serde_json::json!(false);
        let block = render(&p, "127.0.0.1:8182", false);
        assert!(block.contains("qconnect\n  state     off\n"), "{block}");
    }

    #[test]
    fn the_formatters_round_the_way_a_reader_expects() {
        assert_eq!(fmt_hz(44_100), "44.1 kHz");
        assert_eq!(fmt_hz(192_000), "192 kHz");
        assert_eq!(fmt_bytes(2 * 1024 * 1024), "2 MB");
        assert_eq!(fmt_bytes(512 * 1024), "512 KB");
        assert_eq!(fmt_bytes(1_288_490_188), "1.2 GB");
        assert_eq!(fmt_ms(500), "500 ms");
        assert_eq!(fmt_ms(6_000), "6.0 s");
        assert_eq!(fmt_tracks(1), "1 track");
        assert_eq!(fmt_tracks(0), "0 tracks");
    }
}
