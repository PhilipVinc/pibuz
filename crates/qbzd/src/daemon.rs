// crates/qbzd/src/daemon.rs — the `qbzd run` boot sequence (01-architecture.md
// §8.1, NORMATIVE order) and the graceful shutdown (§8.2). Later tasks splice into the numbered steps: the
// playback driver (T4) at step 10, the HTTP server (T6) at step 11, QConnect
// (T9/T10) at step 12. Until they land the daemon boots a playable core and
// parks on signals — API-less but fully diagnosable in-process.
use std::sync::{Arc, Mutex};

use qbz_app::playback_driver::{self, DriverDeps};
use qbz_app::settings::daemon_prefs;
use qbz_app::shell::AppRuntime;
use qbz_models::CoreEvent;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::adapter::DaemonAdapter;
use crate::config::QbzdConfig;
use crate::lock::{InstanceLock, LockError};
use crate::paths::ProfileRoots;
use crate::state::{DaemonShared, LatchedErrors, QconnectStatus};

/// The composed runtime handoff produced by [`boot`] and consumed by later
/// tasks: T4 spawns the playback driver on `runtime` + `shared`, T6 serves
/// `bus` over HTTP/SSE, T9/T10 wire QConnect. Held alive by [`run`] through the
/// signal park so the core stays up.
#[allow(dead_code)] // fields are the seam later tasks (T6/T9/T10) read.
pub struct BootedRuntime {
    pub runtime: Arc<AppRuntime<DaemonAdapter>>,
    pub shared: Arc<Mutex<DaemonShared>>,
    pub bus: broadcast::Sender<CoreEvent>,
    /// Background session-restore retry (network-class boot failure only). Held
    /// so shutdown can abort+join it BEFORE releasing the audio device: it holds
    /// an `Arc<AppRuntime>` clone, so leaving it running would keep the Player
    /// alive past `drop(booted)` and break the #521 clock-release ordering (§8.2).
    pub auth_retry: Option<JoinHandle<()>>,
}

/// `qbzd run` — boot the daemon in the foreground, park on signals, shut down
/// gracefully. Returns the process exit code (0 = clean shutdown). `warns` are
/// the unknown-key warnings surfaced by [`QbzdConfig::load`] in `main`.
pub async fn run(roots: ProfileRoots, cfg: QbzdConfig, warns: Vec<String>) -> Result<i32, String> {
    // 1. argv parse happened in main(). 2. logging:
    qbz_log::install(&cfg.log.level);
    // 3. config: surface unknown-key warnings (they never abort — D14).
    for w in &warns {
        log::warn!("[config] unknown key: {w}");
    }
    // 4. instance lock on the DATA ROOT, taken BEFORE any port bind (§8.3): it,
    //    not the port, protects the single-device_uuid / single-session.db
    //    invariants. A second daemon on the same root is diagnosed → exit 3.
    let _lock = InstanceLock::acquire(&roots.data).map_err(diagnose_lock)?;
    // 5. port bind + foreign-occupant diagnosis — STATELESS, so it runs BEFORE
    //    stores (6) and runtime composition (7) per the §8.1 order. On a bind
    //    conflict the occupant is probed with GET /api/ping: a qbzd answer means
    //    a stale foreign root (the lock said this root was free), anything else
    //    the §2.2 "another process" copy. The socket is bound here but not served
    //    until step 11 — connections queue in the listen backlog through boot.
    let bind_addr = resolve_bind_addr(&cfg)?;
    let bound = match crate::api::bind(bind_addr) {
        Ok(b) => b,
        Err(crate::api::BindError::AddrInUse(addr)) => return Err(diagnose_port_conflict(addr)),
        Err(crate::api::BindError::Other(msg)) => {
            return Err(format!(
                "error: could not bind the control API on {bind_addr}: {msg}\n  → check [server] bind/port in ~/.config/qbzd/qbzd.toml"
            ));
        }
    };
    if !bind_addr.ip().is_loopback() {
        // FB6: the default bind is now 0.0.0.0 — LAN-first posture (Sonos/
        // Chromecast parity), not a misconfiguration. One INFO line, not a
        // stderr warning; loopback binds stay silent.
        log::info!(
            "{}",
            crate::cli::copy::lan_posture_note(&bind_addr.to_string())
        );
    }

    // 6.-9. compose the daemon-root stores and the runtime.
    let mut booted = boot(&roots, &cfg, warns.len()).await?;

    // Attach the CoreEvent bus to the shared state so the qconnect lifecycle
    // latches can publish `QconnectSessionChanged` (SSE + the event hook).
    if let Ok(mut s) = booted.shared.lock() {
        s.bus = Some(booted.bus.clone());
    }

    // 10. playback driver (T4). Spawn the 450 ms headless orchestrator on the
    //     booted runtime + shared state. It runs safely regardless of auth: with
    //     no session the queue is empty and each tick is a near-no-op. The
    //     streaming quality is resolved from daemon_prefs through the SAME key
    //     contract the desktop uses (playback_quality(), playback.rs:170-172),
    //     so hi-res never silently downgrades. 11. HTTP serve (T6) · 12. QConnect
    //     (T9/T10) splice after this, reading `booted`.
    let prefs = daemon_prefs::load_at(&roots.data);
    let quality = playback_driver::quality_from_key(&prefs.streaming_quality);
    // T11: a live-updatable cell, not a value captured once — `settings/reload`
    // re-reads `daemon_prefs` and writes here so the driver's OWN auto-advance
    // (gapless prefetch, natural-end advance) picks up a `playback.quality`
    // change without a restart. Manual play/next/prev already re-read
    // `daemon_prefs` fresh every call (api/playback.rs::resolve_quality); this
    // cell is what makes the BACKGROUND driver loop equally live.
    let quality_cell = Arc::new(std::sync::Mutex::new(quality));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // T10 (§7.2): the driver's `ReportEdge` action pulses this Notify; the
    // QConnect report scheduler (step 12) waits on it. Created BEFORE the driver
    // so `on_edge` can capture it, and shared with `qconnect::start`.
    let report_notify = Arc::new(tokio::sync::Notify::new());
    // The same edge, pulsed onto a SECOND Notify for the events bridge (10e):
    // two subscribers on one Notify would race for the single stored permit.
    let edge_notify = Arc::new(tokio::sync::Notify::new());
    let deps = build_driver_deps(
        quality_cell.clone(),
        booted.shared.clone(),
        report_notify.clone(),
        edge_notify.clone(),
        booted.bus.clone(),
    );
    let driver = tokio::spawn(playback_driver::run_driver(
        booted.runtime.clone(),
        deps,
        shutdown_rx,
    ));

    // 10c½. Events bridge: translate the driver's transition edges into the
    //       playback CoreEvents the bus consumers above (and SSE, and the event
    //       hook below) are written for — TrackStarted / PlaybackStateChanged /
    //       PositionUpdated / VolumeChanged. Holds only a Weak<AppRuntime>
    //       upgraded per wake, but is still aborted+joined ahead of
    //       `drop(booted)` so a mid-wake strong Arc can't outlive the ordering.
    let events_bridge =
        crate::events_bridge::spawn(&booted.runtime, booted.bus.clone(), edge_notify);

    // 10c¾. Event hook (CONSOLE): fork `hooks.script` (or `QBZD_HOOK`) once per
    //       forwarded bus event with QBZ_* variables in its environment — push
    //       integration for headless boxes (moOde-style renderer coordination).
    //       Holds no Arc<AppRuntime>; aborted for a clean shutdown.
    let hooks = match crate::hooks::script(&roots) {
        Some(path) => {
            log::info!(
                "[hooks] running {} on daemon events (hooks.script)",
                path.display()
            );
            Some(crate::hooks::spawn(path, booted.bus.subscribe()))
        }
        None => None,
    };

    // 10d. MPRIS media controls (CONSOLE): publish org.mpris.MediaPlayer2 so a
    //      KDE/GNOME media widget, a plasmoid, or hardware media keys drive the
    //      daemon with no custom client. The inbound callback holds a
    //      Weak<AppRuntime> (never pins the runtime), so it too sits outside the
    //      #521 ordering; None on a headless box / when QBZD_MPRIS disables it.
    let mpris = crate::mpris::spawn(
        &booted.runtime,
        roots.clone(),
        booted.bus.subscribe(),
        tokio::runtime::Handle::current(),
    );

    // 11. HTTP serve (02 §3) on the already-bound socket. `ApiState` carries a
    //     second read-only audio-store connection (WAL) for the status audio
    //     block, the tokio handle for the async queue read, and the opt-in
    //     [server] token (None = open). 12. QConnect (T9/T10) splices after this.
    let api_audio = qbz_audio::settings::AudioSettingsStore::new_at(&roots.data)
        .map_err(|e| format!("error: could not open the audio settings store for the API: {e}"))?;
    // T11: the reload handler's "did a routing-critical field change" diff
    // needs a starting point — seed it from what's on disk right now (the same
    // settings the Player was constructed with at step 6/7).
    let initial_audio_settings = api_audio.get_settings().unwrap_or_default();
    // T11: the running QConnect service is not constructed until step 12
    // (AFTER the API starts serving, per the normative boot order, 01 §8.1) —
    // this cell lets the reload route reach it anyway: empty until `qconnect
    // ::start` below populates it, harmlessly no-op'd by the reload handler in
    // the vanishingly small window before that.
    let qconnect_control: Arc<std::sync::OnceLock<crate::qconnect::QconnectControl>> =
        Arc::new(std::sync::OnceLock::new());
    let api = crate::api::serve(
        bound,
        crate::api::ApiState {
            runtime: booted.runtime.clone(),
            shared: booted.shared.clone(),
            bus: booted.bus.clone(),
            roots: roots.clone(),
            token: cfg.server.token.filter(|t| !t.trim().is_empty()),
            bind: bind_addr.to_string(),
            rt: tokio::runtime::Handle::current(),
            audio: api_audio,
            devices: std::sync::Mutex::new(crate::api::DeviceCache::default()),
            audio_snapshot: std::sync::Mutex::new(initial_audio_settings),
            quality: quality_cell.clone(),
            qconnect_control: qconnect_control.clone(),
        },
    );
    log::info!("control API listening on {bind_addr}");

    // 12. QConnect (T9): mint the daemon's OWN device identity in the daemon-root
    //     KV, decide auto-connect from the persisted startup mode (cli_override =
    //     None so the KV that `qbzd qconnect enable|disable` writes is never
    //     shadowed), and — when enabled — connect-on-Ready with the bounded retry
    //     schedule. Reads NOTHING from qbzd.toml. Held to shut the session down
    //     ahead of playback (§8.2-1); it also clones `Arc<AppRuntime>`, so it must
    //     drop before `drop(booted)` (the #521 ordering).
    let mut qconnect = crate::qconnect::start(
        booted.runtime.clone(),
        booted.shared.clone(),
        &roots,
        report_notify,
        booted.bus.subscribe(),
    );
    // T11: publish the reload route's handle onto the running service now that
    // it exists (`connect`/`disconnect`/device-name refresh — see
    // `qconnect::QconnectControl`).
    let _ = qconnect_control.set(qconnect.control());

    // 13. park on SIGTERM/SIGINT. NO startup audio "hygiene": both candidate
    //     fns are verified no-ops from a fresh process and re-adding them is the
    //     documented skeptic-correction #1 trap (§8.1).
    wait_for_signal().await;

    // ── Shutdown (§8.2, ordered). Step 1: disconnect the QConnect session (and
    //    stop its auto-connect watcher) BEFORE playback is stopped, then drop the
    //    handle so its Arc<AppRuntime> clone is released ahead of `drop(booted)`.
    qconnect.shutdown().await;
    drop(qconnect);
    // Step 2: stop the playback driver. It holds an Arc<AppRuntime> clone, so its
    //    task must finish (dropping that Arc) before `drop(booted)` can release
    //    the audio device ahead of the #521 pair. Signal, then join.
    let _ = shutdown_tx.send(true);
    if let Err(e) = driver.await {
        log::warn!("driver task join failed: {e:?}");
    }
    // T10 (§7.5): stop the queue-persistence subscriber before the authoritative
    // final save, so it neither races the flush below nor keeps its
    // `Arc<AppRuntime>` clone alive past `drop(booted)` (#521 ordering).
    // Stop the events bridge BEFORE `drop(booted)`: it upgrades its Weak to a
    // strong Arc<AppRuntime> for the span of each wake (#521 ordering).
    events_bridge.abort();
    let _ = events_bridge.await;
    // Stop the event-hook dispatcher (holds no Arc<AppRuntime>; order-free).
    if let Some(hooks) = hooks {
        hooks.abort();
        let _ = hooks.await;
    }
    // Tear down MPRIS: abort its updater and drop the D-Bus handle. Its inbound
    // callback held only a Weak<AppRuntime>, so this is order-free too.
    if let Some(mpris) = mpris {
        mpris.shutdown().await;
    }
    // The background auth-retry task also holds an Arc<AppRuntime> clone — abort
    // AND join it so its Arc is dropped before `drop(booted)`; otherwise the
    // ordering claim below (drop releases the device) breaks once playback has
    // engaged a real device.
    if let Some(retry) = booted.auth_retry.take() {
        retry.abort();
        let _ = retry.await;
    }
    // Stop the API thread and JOIN it: its `ApiState` holds an `Arc<AppRuntime>`
    // clone, which must drop before `drop(booted)` releases the audio device
    // ahead of the #521 pair — the same ordering constraint as the driver and
    // auth-retry tasks (§8.2).
    api.shutdown();
    // The reload route's OnceLock handle also clones `QconnectControl`, which
    // holds an `Arc<AppRuntime>` (via `DaemonQconnectService.runtime`) — drop
    // it before `drop(booted)` too, same #521/§8.2 ordering as the driver,
    // queue-persist and auth-retry tasks above.
    drop(qconnect_control);
    // Release the audio device by dropping the runtime (its Player) BEFORE the
    // #521 pair (§8.2 step 3 precedes step 4).
    drop(booted);
    //    THE #521 PAIR runs unconditionally on Linux: a forced PipeWire clock
    //    left set would pin the whole system's sample rate after the process
    //    dies.
    //    Both calls self-gate to no-ops when QBZ forced nothing.
    #[cfg(target_os = "linux")]
    {
        qbz_audio::alsa_backend::resume_suspended_sink();
        qbz_audio::pipewire_backend::PipeWireBackend::reset_pipewire_clock();
    }

    Ok(0) // instance lock released on drop of `_lock`
}

/// Steps 6-9 of §8.1: open the daemon-root stores and compose the runtime.
async fn boot(
    roots: &ProfileRoots,
    cfg: &QbzdConfig,
    warn_count: usize,
) -> Result<BootedRuntime, String> {
    // 6.+7. stores + runtime composition. `with_audio_settings` takes the
    // settings the caller already opened, so everything routes through the T2
    // daemon roots rather than any global path.
    let store = qbz_audio::settings::AudioSettingsStore::new_at(&roots.data)?; // settings.rs:263
    let settings = store.get_settings()?;
    let (adapter, _rx) = DaemonAdapter::new();
    let bus = adapter.sender();
    let runtime = Arc::new(AppRuntime::with_audio_settings(
        adapter,
        settings.output_device.clone(),
        settings,
    ));

    // Offline-tolerant (§8.1-8): a network failure here still leaves a locally
    // usable core; a missing DAC is likewise non-fatal (Player starts deviceless
    // and retries with backoff — never the spotifyd #1097 crash-exit).
    if let Err(e) = runtime.init().await {
        log::warn!("core init did not complete (continuing offline-tolerant): {e}");
    }

    // Playlist recommendations (CONSOLE): open the per-user artist-vector store
    // at the DAEMON root — mirrors qbz/src/auth.rs:145-149, but slint-free and
    // session-independent. The store is a CACHE the suggestions engine reads/
    // writes; vectors are built on demand from MusicBrainz + Qobuz, so this
    // needs no listening history. Best-effort: a failed open leaves
    // `generate_playlist_suggestions` working un-cached (artist_vectors = None).
    let shared = new_shared(cfg);
    if let Ok(mut s) = shared.lock() {
        s.startup_warnings = warn_count as u32;
    }

    // 8. No credential restore: this daemon has no account path. A Qobuz
    //    Connect handoff supplies its own `jwt_api` streaming credential
    //    (see qconnect/pairing.rs), so there is nothing on disk to restore
    //    and nothing to retry — the renderer simply waits to be cast to.
    let auth_retry = None;

    Ok(BootedRuntime {
        runtime,
        shared,
        bus,
        auth_retry,
    })
}

/// Assemble the driver's host side channels: the streaming-quality resolver and
/// the daemon-shared latching / tick-timestamping hooks. T10: `on_edge` now
/// pulses the QConnect report `Notify` so the report scheduler reports on the
/// same transition/periodic edges the driver detects (§7.2) — and, on a second
/// `Notify`, the events bridge (10c½) that publishes those edges to the bus.
fn build_driver_deps(
    quality_cell: Arc<std::sync::Mutex<qbz_models::Quality>>,
    shared: Arc<Mutex<DaemonShared>>,
    report_notify: Arc<tokio::sync::Notify>,
    edge_notify: Arc<tokio::sync::Notify>,
    bus: tokio::sync::broadcast::Sender<qbz_models::CoreEvent>,
) -> DriverDeps {
    let latch_shared = shared.clone();
    let tick_shared = shared;
    DriverDeps {
        quality: Arc::new(move || {
            quality_cell
                .lock()
                .map(|q| *q)
                .unwrap_or(qbz_models::Quality::UltraHiRes)
        }),
        // T10: signal the report scheduler on every ReportEdge. `notify_one`
        // stores a single permit if the scheduler is mid-report, so no edge is
        // lost and rapid edges coalesce into one report.
        on_edge: Arc::new(move || {
            report_notify.notify_one();
            edge_notify.notify_one();
        }),
        on_latch: Arc::new(move |category, message| {
            if let Ok(mut s) = latch_shared.lock() {
                match category {
                    "stream" => s.last_errors.stream = Some(message.clone()),
                    "transport" => s.last_errors.transport = Some(message.clone()),
                    "auth" => s.last_errors.auth = Some(message.clone()),
                    _ => {}
                }
            }
            // Surface stream failures on the bus too, so `/api/events` and the
            // event hook see them live (e.g. a busy ALSA device at play time)
            // instead of only the polled /api/status error latch.
            if category == "stream" {
                let _ = bus.send(qbz_models::CoreEvent::PlaybackError {
                    track_id: 0,
                    message,
                });
            }
        }),
        on_tick: Arc::new(move || {
            if let Ok(mut s) = tick_shared.lock() {
                s.driver_last_tick = Some(std::time::Instant::now());
            }
        }),
    }
}

/// Fresh shared state.
fn new_shared(cfg: &QbzdConfig) -> Arc<Mutex<DaemonShared>> {
    let _ = cfg; // reserved: premute/mpris defaults wire in with later tasks.
    Arc::new(Mutex::new(DaemonShared {
        last_errors: LatchedErrors::default(),
        driver_last_tick: None,
        muted: false,
        premute_volume: 1.0,
        started_at: std::time::Instant::now(),
        startup_warnings: 0,
        qconnect: QconnectStatus::default(),
        network_online: std::sync::atomic::AtomicBool::new(true),
        // Attached by daemon::run right after boot (the bus outlives boot).
        bus: None,
    }))
}

/// Resolve `[server] bind:port` to a `SocketAddr` (01 §10.1). A malformed value
/// is a fatal boot error that names the fix (exit 1).
fn resolve_bind_addr(cfg: &QbzdConfig) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
    let hostport = format!("{}:{}", cfg.server.bind, cfg.server.port);
    hostport
        .to_socket_addrs()
        .map_err(|e| {
            format!("error: invalid [server] bind/port '{hostport}': {e}\n  → set a valid ip and port in ~/.config/qbzd/qbzd.toml")
        })?
        .next()
        .ok_or_else(|| {
            format!("error: [server] '{hostport}' resolved to no address\n  → set a valid ip and port in ~/.config/qbzd/qbzd.toml")
        })
}

/// The step-5 bind-conflict diagnosis (02 §8.1-5 / §2.2): a qbzd occupant on a
/// different data root vs. an unrelated process on the port.
fn diagnose_port_conflict(addr: std::net::SocketAddr) -> String {
    if crate::api::probe_is_qbzd(addr) {
        crate::cli::copy::foreign_qbzd(&addr.to_string())
    } else {
        crate::cli::copy::port_in_use(addr.port())
    }
}

/// Render an [`InstanceLock`] failure. For the already-running case this prints
/// the frozen exit-3 error voice (02 §1.3/§1.4) and exits 3 directly — the new
/// process must never clobber the running one. An I/O failure returns a String
/// that propagates to a generic exit 1.
fn diagnose_lock(e: LockError) -> String {
    match e {
        LockError::AlreadyRunning(pid) => {
            let who = pid
                .map(|p| format!("(pid {p})"))
                .unwrap_or_else(|| "(pid unknown)".to_string());
            eprintln!("error: qbzd is already running {who}");
            eprintln!("  → stop it first:  systemctl --user stop qbzd");
            eprintln!("  → or inspect it:  systemctl --user status qbzd");
            std::process::exit(3);
        }
        LockError::Io(msg) => {
            format!("error: could not take the instance lock: {msg}\n  → check permissions on the data root")
        }
    }
}

/// Park until SIGTERM or SIGINT. A second signal after this returns lets the
/// default handler take over → immediate exit (§8.2).
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::select! {
                    _ = term.recv() => log::info!("SIGTERM received — shutting down"),
                    _ = int.recv()  => log::info!("SIGINT received — shutting down"),
                }
            }
            _ => {
                // Fall back to Ctrl-C if the SIGTERM handler could not install.
                let _ = tokio::signal::ctrl_c().await;
                log::info!("Ctrl-C received — shutting down");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        log::info!("Ctrl-C received — shutting down");
    }
}

// ============================ T11: settings reload ============================
// `POST /api/settings/reload` (02-cli-and-api.md §3.3.17; `crate::api::settings
// ::reload` is the thin HTTP wrapper) re-reads every engine store and applies
// what changed: audio (routing-critical -> `Player::reinit_device`, the rest ->
// `Player::reload_settings`), the daemon's own streaming-quality cell (the
// driver's background auto-advance), the QConnect KV (device-name cache +
// connect/disconnect reconciliation). Never re-reads `qbzd.toml` (§3.1.2 —
// process config is boot-only). Response = the post-reload `/api/status` body,
// composed by the caller — zero new shapes (03-setup-tui.md §4.3: the
// reinit/reload narrative is composed CLIENT-side from the CLI's own copy of
// the Apply-ladder classification, never carried on the wire).

/// The single entry point the HTTP route calls. Order matters only at the
/// margin (independent domains): audio/quality/qconnect-KV first, credentials
/// last, so a login/logout settles the auth state before QConnect decides
/// whether to (re)connect against it.
pub(crate) async fn reload(state: &crate::api::ApiState) {
    reload_audio(state);
    reload_quality(state);
    reload_qconnect(state).await;
}

/// Re-read `audio_settings.db` and apply it to the live `Player`. A struct
/// refresh (`reload_settings`) always happens; the output device is ADDITIONALLY
/// reinitialized only when a routing-critical field actually changed since the
/// last reload (mirrors the desktop's `Apply::Reinit` — `qbz/src/settings.rs:
/// 87-94`, per-key classification `:877-967,1134-1290`; 03-setup-tui.md §4.3
/// lists the same 9 fields).
pub(crate) fn reload_audio(state: &crate::api::ApiState) {
    let fresh = match state.audio.get_settings() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[reload] could not re-read audio settings: {e}");
            return;
        }
    };
    let player = state.runtime.core().player();
    if let Err(e) = player.reload_settings(fresh.clone()) {
        log::warn!("[reload] player.reload_settings failed: {e}");
    }
    let needs_reinit = state
        .audio_snapshot
        .lock()
        .map(|old| audio_routing_changed(&old, &fresh))
        .unwrap_or(false);
    if needs_reinit {
        log::info!(
            "[reload] routing-critical audio field changed — reinitializing the output device"
        );
        if let Err(e) = player.reinit_device(fresh.output_device.clone()) {
            log::warn!("[reload] player.reinit_device failed: {e}");
        }
    }
    if let Ok(mut snap) = state.audio_snapshot.lock() {
        *snap = fresh;
    }
}

/// The Reinit-class field set (03-setup-tui.md §4.3 / `qbz/src/settings.rs:
/// 877-967,1134-1290`): backend, device, ALSA plugin, DSD mode, max sample
/// rate, exclusive mode, DAC passthrough, hardware volume, lock-output
/// (`skip_sink_switch`). Every other `AudioSettings` field is Reload-class —
/// `player.reload_settings` above already covers it unconditionally.
pub(crate) fn audio_routing_changed(
    old: &qbz_audio::settings::AudioSettings,
    new: &qbz_audio::settings::AudioSettings,
) -> bool {
    old.backend_type != new.backend_type
        || old.output_device != new.output_device
        || old.alsa_plugin != new.alsa_plugin
        || old.alsa_hardware_volume != new.alsa_hardware_volume
        || old.exclusive_mode != new.exclusive_mode
        || old.dac_passthrough != new.dac_passthrough
        || old.skip_sink_switch != new.skip_sink_switch
        || old.device_max_sample_rate != new.device_max_sample_rate
}

/// Re-read `daemon_prefs.streaming_quality` into the live cell the driver's
/// background auto-advance reads (`daemon.rs::run`'s `quality_cell`). Manual
/// play/next/prev already re-read `daemon_prefs` fresh every call
/// (`api/playback.rs::resolve_quality`); this is what makes the passive
/// natural-end-of-track advance equally live.
pub(crate) fn reload_quality(state: &crate::api::ApiState) {
    let prefs = daemon_prefs::load_at(&state.roots.data);
    let fresh = playback_driver::quality_from_key(&prefs.streaming_quality);
    if let Ok(mut q) = state.quality.lock() {
        *q = fresh;
    }
}

/// Re-cache the QConnect device-name override from the daemon-root KV (so the
/// NEXT connect uses whatever `qbzd qconnect name` / `settings set
/// qconnect.device_name` most recently wrote — 03-setup-tui.md §3.4: "applies
/// on the next connection", never forcing a reconnect just for a rename), then
/// reconcile the connect/disconnect state against the freshly-read
/// `startup_mode` (`qbzd qconnect enable|disable` — idempotent either way, see
/// `qconnect::QconnectControl`). A no-op before step 12 populates the cell.
pub(crate) async fn reload_qconnect(state: &crate::api::ApiState) {
    let Some(qc) = state.qconnect_control.get() else {
        return;
    };
    let db = state.roots.data.join("qconnect_settings.db");
    qc.refresh_device_name(&db).await;
    let mode = crate::qconnect::transport::load_startup_mode_at(&db);
    let should_connect = qconnect_app::compute_effective_startup(mode, None, None);
    if should_connect {
        if let Err(e) = qc.connect().await {
            log::info!("[reload] qconnect connect deferred: {e}");
        }
    } else if let Err(e) = qc.disconnect().await {
        log::warn!("[reload] qconnect disconnect failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_shared_state_has_no_latched_errors() {
        let shared = new_shared(&QbzdConfig::default());
        let s = shared.lock().unwrap();
        assert!(s.last_errors.auth.is_none());
        assert!(s.last_errors.stream.is_none());
        assert!(s.last_errors.transport.is_none());
    }

    // ======================= T11: settings/reload =======================

    fn base_audio_settings() -> qbz_audio::settings::AudioSettings {
        qbz_audio::settings::AudioSettings::default()
    }

    #[test]
    fn audio_routing_changed_false_when_nothing_moved() {
        let a = base_audio_settings();
        let b = base_audio_settings();
        assert!(!audio_routing_changed(&a, &b));
    }

    #[test]
    fn audio_routing_changed_true_for_each_reinit_class_field() {
        let base = base_audio_settings();

        let mut backend = base.clone();
        backend.backend_type = Some(qbz_audio::AudioBackendType::Alsa);
        assert!(audio_routing_changed(&base, &backend), "backend_type");

        let mut device = base.clone();
        device.output_device = Some("hw:CARD=D30,DEV=0".into());
        assert!(audio_routing_changed(&base, &device), "output_device");

        let mut plugin = base.clone();
        plugin.alsa_plugin = Some(qbz_audio::AlsaPlugin::PlugHw);
        assert!(audio_routing_changed(&base, &plugin), "alsa_plugin");

        let mut hw_vol = base.clone();
        hw_vol.alsa_hardware_volume = !base.alsa_hardware_volume;
        assert!(
            audio_routing_changed(&base, &hw_vol),
            "alsa_hardware_volume"
        );

        let mut excl = base.clone();
        excl.exclusive_mode = !base.exclusive_mode;
        assert!(audio_routing_changed(&base, &excl), "exclusive_mode");

        let mut pass = base.clone();
        pass.dac_passthrough = !base.dac_passthrough;
        assert!(audio_routing_changed(&base, &pass), "dac_passthrough");

        let mut lock_out = base.clone();
        lock_out.skip_sink_switch = !base.skip_sink_switch;
        assert!(audio_routing_changed(&base, &lock_out), "skip_sink_switch");

        let mut rate = base.clone();
        rate.device_max_sample_rate = Some(192_000);
        assert!(
            audio_routing_changed(&base, &rate),
            "device_max_sample_rate"
        );
    }

    #[test]
    fn audio_routing_changed_false_for_reload_class_fields_only() {
        // Changing ONLY Reload-class fields must never trip a reinit.
        let base = base_audio_settings();
        let mut reload_only = base.clone();
        reload_only.gapless_enabled = !base.gapless_enabled;
        reload_only.stream_first_track = !base.stream_first_track;
        reload_only.stream_buffer_seconds = 7;
        reload_only.streaming_only = !base.streaming_only;
        reload_only.limit_quality_to_device = !base.limit_quality_to_device;
        reload_only.allow_quality_fallback = !base.allow_quality_fallback;
        reload_only.quality_fallback_behavior = "always_skip".to_string();
        reload_only.normalization_enabled = !base.normalization_enabled;
        reload_only.normalization_target_lufs = -18.0;
        reload_only.pw_force_bitperfect = !base.pw_force_bitperfect;
        reload_only.reserve_dac_while_running = !base.reserve_dac_while_running;
        reload_only.sync_audio_on_startup = !base.sync_audio_on_startup;
        assert!(!audio_routing_changed(&base, &reload_only));
    }
}
