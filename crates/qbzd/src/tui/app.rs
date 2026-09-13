// crates/qbzd/src/tui/app.rs — the setup-TUI state machine.
//
// Owns the eight screens (the D7 six-screen cap was lifted for FB4's Wizard and
// the CONSOLE ext's Scrobbler, both owner-sanctioned), the route,
// the dirty-save model (§4), the App-level overlays (help / result panel /
// dirty-leave modal), and the worker plumbing (§5.5: NO I/O on keystrokes — disk
// and HTTP happen only at screen entry, `r`, save, and the immediate actions, on
// a worker with a spinner). Persistence is reused wholesale: saves go through
// T11's `write_one`, import/export through the T12 bundle engine, auth through
// the T5 login engine. The TUI adds no persistence of its own (03 §6).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use serde_json::{json, Value};
use tokio::runtime::Handle;

use qbz_app::settings::bundle::{
    self, Bundle, ExportOptions, ExportSource, ImportOptions, LiveSystem, ProfilePaths,
};
use qbz_app::settings::daemon_prefs;
use qbz_app::settings::playback::PlaybackPreferencesStore;
use qbz_audio::settings::{AudioSettings, AudioSettingsStore};
use qbz_audio::{AudioBackendType, AudioDevice, BackendManager, NegotiatedRate};

use crate::cli::client::{ApiClient, CliError};
use crate::config::QbzdConfig;
use crate::paths::ProfileRoots;
use crate::qconnect::transport as qconnect_kv;

use super::screens::audio::AudioState;
use super::screens::bundle::{BundleState, PendingImport};
use super::screens::network::{self as network_screen, NetworkState};
use super::screens::playback::PlaybackState;
use super::screens::qconnect::QConnectState;
use super::screens::wizard::WizardState;
use super::strings as s;
use super::theme;
use super::widgets;
use super::wizard_core::{self, DacCandidateData, DacConfigData};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

// ============================ shared vocabulary ============================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Audio,
    Playback,
    QConnect,
    Network,
    Bundle,
    Wizard,
}

/// Six sections. Was eight until the account path was removed: this daemon has
/// no Qobuz login and no scrobbling, so the Account and Scrobbler sections went
/// with them. What remains is what `qbzd setup` is now for — configuring the
/// audio device and the renderer.
pub const SCREENS: [Screen; 6] = [
    Screen::Audio,
    Screen::Playback,
    Screen::QConnect,
    Screen::Network,
    Screen::Bundle,
    Screen::Wizard,
];

// Sidebar width is now responsive (FB5): `widgets::sidebar_width(term_width)` —
// 28 cols at ≥ 100 (roomy: spelled-out labels + a dim summary line), 14 below so
// the 80×24 floor keeps its 64-col content frame.

/// Which pane holds the keyboard focus. The frames shell has two: the persistent
/// left navigation sidebar and the right content frame (FB3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    Content,
}

/// The 0-based index of a section in `SCREENS` (sidebar row / number key).
fn section_index(screen: Screen) -> usize {
    SCREENS.iter().position(|s| *s == screen).unwrap_or(0)
}

/// The full section title for the breadcrumb (the sidebar uses short labels).
fn section_title(screen: Screen) -> &'static str {
    match screen {
        Screen::Audio => s::AUDIO_TITLE,
        Screen::Playback => s::PLAYBACK_TITLE,
        Screen::QConnect => s::QCONNECT_TITLE,
        Screen::Network => s::NETWORK_TITLE,
        Screen::Bundle => s::BUNDLE_TITLE,
        Screen::Wizard => s::WIZARD_TITLE,
    }
}

/// Breadcrumb node composition (max 2 levels, FB3). Pure so it can be pinned:
/// - not editing → (`Setup`, section)   — dim prefix, accent current.
/// - editing a field → (section, field) — the field label is the current node.
///   Modals/pickers are the third level (overlays); they do NOT change the crumb,
///   so the caller passes `None` for them (see each screen's `editing_label`).
fn breadcrumb_nodes<'a>(section: &'a str, editing_field: Option<&'a str>) -> (&'a str, &'a str) {
    match editing_field {
        Some(field) => (section, field),
        None => (s::BREADCRUMB_ROOT, section),
    }
}

/// Whether a sidebar row shows the dirty `*`. Only the ACTIVE section can be
/// dirty — leaving a section is gated by the Save/Discard/Stay modal, so every
/// other section is clean by construction. Pure for the mapping test.
fn sidebar_dirty_marker(row: Screen, active: Screen, active_dirty: bool) -> bool {
    row == active && active_dirty
}

/// A key's navigation meaning, resolved purely from the focus state (FB3
/// focus-transition table). The impure `on_key` executes the intent (dirty
/// guards, section loads, screen dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavIntent {
    /// Nav: no-op (unbound key).
    None,
    /// Nav: move the sidebar highlight by ±1 (wrapping).
    MoveCursor(isize),
    /// Nav: activate the highlighted section and focus the content.
    ActivateCursor,
    /// Any focus (not editing): jump straight to section `idx` and focus content.
    JumpSection(usize),
    /// Content: drop focus back to the nav.
    FocusNav,
    /// Nav Esc/q or content q: the quit flow (dirty-guarded).
    Quit,
    /// The help overlay.
    Help,
    /// Content: hand the key to the active screen (field navigation / edit).
    ToScreen,
}

/// Map a keystroke to its `NavIntent` (FB3). `editing` = a field editor/picker
/// is open in the content (it owns every key); `uses_horizontal` = the focused
/// content field consumes ←/→ (Audio's Buffer slider), so ← must NOT drop focus.
fn classify_key(focus: Focus, code: KeyCode, editing: bool, uses_horizontal: bool) -> NavIntent {
    // Number keys 1-8 jump from ANY focus — but only when no field editor is
    // capturing input (a port/token/name field must receive its digits).
    if !editing {
        if let KeyCode::Char(c @ '1'..='8') = code {
            return NavIntent::JumpSection(c as usize - '1' as usize);
        }
    }
    match focus {
        Focus::Nav => match code {
            KeyCode::Up | KeyCode::Char('k') => NavIntent::MoveCursor(-1),
            KeyCode::Down | KeyCode::Char('j') => NavIntent::MoveCursor(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Tab => NavIntent::ActivateCursor,
            KeyCode::Esc | KeyCode::Char('q') => NavIntent::Quit,
            KeyCode::Char('?') => NavIntent::Help,
            _ => NavIntent::None,
        },
        Focus::Content => {
            // An open editor owns the keyboard (Esc cancels the edit, digits type,
            // etc.) — nothing is intercepted for focus changes.
            if editing {
                return NavIntent::ToScreen;
            }
            match code {
                KeyCode::Tab => NavIntent::FocusNav,
                // ← walks left toward the sidebar unless the field claims it.
                KeyCode::Left if !uses_horizontal => NavIntent::FocusNav,
                KeyCode::Char('?') => NavIntent::Help,
                KeyCode::Char('q') => NavIntent::Quit,
                // Everything else (↑↓, s, r, /, Enter, →, Esc→Back) is the
                // screen's. Esc returns `ScreenAction::Back`, which the App maps
                // to FocusNav — so Esc in content also lands on the sidebar.
                _ => NavIntent::ToScreen,
            }
        }
    }
}

/// The intent a screen's key handler returns to the App.
pub enum ScreenAction {
    Consumed,
    Save,
    Back,
    RefreshDevices,
    ImportPlan(String),
    ImportApply,
    Export {
        dest: String,
        include_auth: bool,
    },
    // ---- HiFi Wizard (FB4) worker requests ----
    /// Run the heavy audio-stack health probe (Check step).
    WizardProbeHealth,
    /// Enumerate DAC candidates via pw-dump (Select-DACs step).
    WizardDetect,
    /// Generate the per-DAC config blocks (Review step).
    WizardGenConfigs(Vec<(String, String)>),
    /// Start the daemon's own playback of the current queue, then read back.
    WizardTestStart,
    /// Re-read the requested-vs-negotiated rate without (re)starting playback.
    WizardTestPoll,
    /// Esc mid-wizard — open the confirm-abandon modal.
    WizardAbandon,
}

/// Read-only context passed to every screen's `draw` (the live status body for
/// the screens that render a daemon-state line).
pub struct DrawCtx<'a> {
    pub status: Option<&'a Value>,
}

/// What the event loop must do after handling a key (terminal-control cases).
pub enum LoopCmd {
    None,
}

// ============================ worker messages ============================

pub enum Msg {
    Devices(Result<Vec<AudioDevice>, String>),
    Saved {
        lines: Vec<String>,
        status: Option<Value>,
        reachable: bool,
        success: bool,
    },
    ImportPlanned(Result<Box<PendingImport>, String>),
    ImportApplied {
        lines: Vec<String>,
        status: Option<Value>,
        reachable: bool,
    },
    Exported(Result<Vec<String>, String>),
    // ---- HiFi Wizard (FB4) worker results ----
    WizardHealth(qbz_audio::AudioStackHealth),
    WizardDacs(Vec<DacCandidateData>),
    WizardConfigs(Vec<DacConfigData>),
    WizardTest {
        requested: Option<(u32, u32)>,
        negotiated: Option<NegotiatedRate>,
        note: Option<String>,
    },
}

// ============================ active screen ============================

// One value alive at a time for the life of the TUI — boxing a variant would
// add an allocation per screen switch to save stack this program has no
// shortage of.
#[allow(clippy::large_enum_variant)]
enum Active {
    Audio(AudioState),
    Playback(PlaybackState),
    QConnect(QConnectState),
    Network(NetworkState),
    Bundle(BundleState),
    Wizard(WizardState),
}

enum Overlay {
    None,
    Help,
    Result {
        title: String,
        lines: Vec<String>,
    },
    DirtyLeave {
        target: LeaveTarget,
    },
    /// FB4: Esc mid-wizard — leaving discards the wizard's transient selections.
    ConfirmAbandon,
}

/// Where a dirty-guarded departure lands. Switching sections and quitting both
/// route through the SAME Save/Discard/Stay modal (FB3 — the modal is verbatim
/// the pre-FB3 one; only the target set changed: `Menu` became `Section`).
#[derive(Clone, Copy)]
enum LeaveTarget {
    Section(Screen),
    Quit,
}

// ============================ App ============================

pub struct App {
    roots: ProfileRoots,
    handle: Handle,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    active: Active,
    /// Which section `active` currently holds (its state is loaded). The sidebar
    /// marks it `▸` + accent; the breadcrumb names it.
    active_section: Screen,
    /// The sidebar highlight while `focus == Nav`. Reset to `active_section` on
    /// every entry into nav focus; moving it does NO I/O (it only re-points the
    /// highlight — a section loads only on activation, §5.5).
    nav_cursor: usize,
    /// Which pane owns the keyboard (FB3 dual focus).
    focus: Focus,

    status: Option<Value>,
    reachable: bool,

    overlay: Overlay,
    busy: Option<String>,
    pub busy_tick: u64,
    should_quit: bool,
    /// Set when a save was requested from the dirty-leave modal — the leave
    /// happens once the save succeeds (§4.1 Save/Discard/Stay → Save then leave).
    leave_after_save: Option<LeaveTarget>,
}

impl App {
    pub fn new(roots: ProfileRoots, handle: Handle) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App {
            roots: roots.clone(),
            handle,
            tx,
            rx,
            active: Active::Audio(AudioState::new(&AudioSettings::default())),
            active_section: Screen::Audio,
            nav_cursor: 0,
            focus: Focus::Nav,
            status: None,
            reachable: false,
            overlay: Overlay::None,
            busy: None,
            busy_tick: 0,
            should_quit: false,
            leave_after_save: None,
        };
        // Landing: the Audio section. This used to be Account, which no longer
        // exists — `qbzd setup` is now an audio/device configurator, and the
        // output device is the first thing anyone needs to set.
        app.enter_screen(Screen::Audio);
        app.focus = Focus::Content;
        app
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
    pub fn busy(&self) -> bool {
        self.busy.is_some()
    }

    // -------------------------- status / auth --------------------------

    fn refresh_status(&mut self) {
        let roots = self.roots.clone();
        let body = self.handle.block_on(fetch_status(roots));
        self.reachable = body.is_some();
        self.status = body;
    }

    // -------------------------- navigation --------------------------

    /// Load a section into the content frame (a §5.5 "screen entry": disk reads
    /// + a fresh daemon-status fetch happen here, never on a keystroke). Sets the
    ///   active section and syncs the sidebar cursor to it.
    fn enter_screen(&mut self, screen: Screen) {
        self.refresh_status();
        self.active_section = screen;
        self.nav_cursor = section_index(screen);
        self.active = match screen {
            Screen::Audio => {
                let audio = load_audio(&self.roots);
                let mut st = AudioState::new(&audio);
                st.start_scan();
                let backend = st.backend();
                self.spawn_devices(backend);
                Active::Audio(st)
            }
            Screen::Playback => {
                let audio = load_audio(&self.roots);
                let playback = PlaybackPreferencesStore::new_at(&self.roots.data)
                    .and_then(|s| s.get_preferences())
                    .unwrap_or_default();
                let dp = daemon_prefs::load_at(&self.roots.data);
                Active::Playback(PlaybackState::new(
                    &dp.streaming_quality,
                    dp.mpris_enabled,
                    &audio,
                    &playback,
                ))
            }
            Screen::QConnect => {
                let db = self.roots.data.join("qconnect_settings.db");
                let on = matches!(
                    qconnect_kv::load_startup_mode_at(&db),
                    qconnect_app::QconnectStartupMode::On
                );
                let name = qconnect_kv::load_device_name_at(&db);
                let vol = qconnect_kv::load_volume_mode_at(&db);
                Active::QConnect(QConnectState::new(on, name, vol))
            }
            Screen::Network => {
                let (cfg, warns) = QbzdConfig::load(&self.roots.config.join("qbzd.toml"))
                    .unwrap_or_else(|_| (QbzdConfig::default(), Vec::new()));
                Active::Network(NetworkState::new(&cfg, warns))
            }
            Screen::Bundle => Active::Bundle(BundleState::new(desktop_profile_present())),
            Screen::Wizard => Active::Wizard(WizardState::new()),
        };
    }

    /// Request a switch to `target` (sidebar activation / number key). Same
    /// section → just move focus to the content (no reload — §5.5). Different
    /// section → the dirty guard fires (Save/Discard/Stay) before the load.
    fn request_section(&mut self, target: Screen) {
        if target == self.active_section {
            self.focus = Focus::Content;
            return;
        }
        if self.active_is_dirty() {
            self.overlay = Overlay::DirtyLeave {
                target: LeaveTarget::Section(target),
            };
            return;
        }
        self.enter_screen(target);
        self.focus = Focus::Content;
    }

    /// The quit flow, dirty-guarded (Esc/q in nav, q in content). A dirty active
    /// section opens the Save/Discard/Stay modal targeting `Quit`.
    fn leave_quit(&mut self) {
        if self.active_is_dirty() {
            self.overlay = Overlay::DirtyLeave {
                target: LeaveTarget::Quit,
            };
        } else {
            self.should_quit = true;
        }
    }

    /// Move focus to the sidebar (content → nav), re-seating the highlight on the
    /// active section.
    fn enter_nav_focus(&mut self) {
        self.focus = Focus::Nav;
        self.nav_cursor = section_index(self.active_section);
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = SCREENS.len() as isize;
        self.nav_cursor = (self.nav_cursor as isize + delta).rem_euclid(n) as usize;
    }

    /// Execute a dirty-guarded departure once the guard is cleared (or absent).
    fn apply_leave(&mut self, target: LeaveTarget) {
        match target {
            LeaveTarget::Section(screen) => {
                self.enter_screen(screen);
                self.focus = Focus::Content;
            }
            LeaveTarget::Quit => self.should_quit = true,
        }
    }

    fn active_is_dirty(&self) -> bool {
        match &self.active {
            Active::Audio(s) => s.is_dirty(),
            Active::Playback(s) => s.is_dirty(),
            Active::QConnect(s) => s.is_dirty(),
            Active::Network(s) => s.is_dirty(),
            _ => false,
        }
    }

    fn active_is_editing(&self) -> bool {
        match &self.active {
            Active::Audio(s) => s.is_editing(),
            Active::Playback(s) => s.is_editing(),
            Active::QConnect(s) => s.is_editing(),
            Active::Network(s) => s.is_editing(),
            Active::Bundle(s) => s.is_editing(),
            Active::Wizard(s) => s.is_editing(),
        }
    }

    /// The breadcrumb's level-2 node (field label) when an inline edit is active.
    /// The Wizard uses it for the current STEP name (`Wizard › <step>`).
    fn active_editing_label(&self) -> Option<&'static str> {
        match &self.active {
            Active::Audio(s) => s.editing_label(),
            Active::Playback(s) => s.editing_label(),
            Active::QConnect(s) => s.editing_label(),
            Active::Network(s) => s.editing_label(),
            Active::Bundle(s) => s.editing_label(),
            Active::Wizard(s) => s.editing_label(),
        }
    }

    /// Whether the focused content field consumes ←/→ (so ← must not drop focus).
    /// The Wizard claims ←/→ for step back/next navigation.
    fn content_uses_horizontal(&self) -> bool {
        match &self.active {
            Active::Audio(s) => s.focused_is_buffer(),
            Active::Wizard(s) => s.claims_horizontal(),
            _ => false,
        }
    }

    // -------------------------- key handling --------------------------

    pub fn on_key(&mut self, key: KeyEvent) -> LoopCmd {
        if self.busy.is_some() {
            return LoopCmd::None; // §5.5: input parked while a worker runs
        }
        // Overlays capture keys first.
        match &self.overlay {
            Overlay::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
                    self.overlay = Overlay::None;
                }
                return LoopCmd::None;
            }
            Overlay::Result { .. } => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    self.overlay = Overlay::None;
                }
                return LoopCmd::None;
            }
            Overlay::DirtyLeave { target } => {
                let target = *target;
                match key.code {
                    KeyCode::Char('s') => {
                        self.overlay = Overlay::None;
                        self.save_active(Some(target));
                    }
                    KeyCode::Char('d') => {
                        self.overlay = Overlay::None;
                        self.apply_leave(target);
                    }
                    KeyCode::Esc => self.overlay = Overlay::None,
                    _ => {}
                }
                return LoopCmd::None;
            }
            Overlay::ConfirmAbandon => {
                match key.code {
                    // Quit the wizard: discard its transient state (a fresh
                    // WizardState) and drop focus back to the sidebar.
                    KeyCode::Char('y') | KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.enter_screen(Screen::Wizard);
                        self.enter_nav_focus();
                    }
                    KeyCode::Esc | KeyCode::Char('n') => self.overlay = Overlay::None,
                    _ => {}
                }
                return LoopCmd::None;
            }
            Overlay::None => {}
        }

        // FB3 dual focus: resolve the key's navigation meaning purely, then
        // execute it. Content field keys (ToScreen) still flow to the active
        // screen exactly as before.
        let editing = self.active_is_editing();
        let uses_h = self.content_uses_horizontal();
        match classify_key(self.focus, key.code, editing, uses_h) {
            NavIntent::None => LoopCmd::None,
            NavIntent::MoveCursor(d) => {
                self.move_cursor(d);
                LoopCmd::None
            }
            NavIntent::ActivateCursor => {
                self.request_section(SCREENS[self.nav_cursor]);
                LoopCmd::None
            }
            NavIntent::JumpSection(idx) => {
                self.request_section(SCREENS[idx]);
                LoopCmd::None
            }
            NavIntent::FocusNav => {
                self.enter_nav_focus();
                LoopCmd::None
            }
            NavIntent::Quit => {
                self.leave_quit();
                LoopCmd::None
            }
            NavIntent::Help => {
                self.overlay = Overlay::Help;
                LoopCmd::None
            }
            NavIntent::ToScreen => {
                let action = self.dispatch_screen_key(key);
                self.handle_screen_action(action)
            }
        }
    }

    fn dispatch_screen_key(&mut self, key: KeyEvent) -> ScreenAction {
        match &mut self.active {
            Active::Audio(s) => s.handle_key(key),
            Active::Playback(s) => s.handle_key(key),
            Active::QConnect(s) => s.handle_key(key),
            Active::Network(s) => s.handle_key(key),
            Active::Bundle(s) => s.handle_key(key),
            Active::Wizard(s) => s.handle_key(key),
        }
    }

    fn handle_screen_action(&mut self, action: ScreenAction) -> LoopCmd {
        match action {
            ScreenAction::Consumed => LoopCmd::None,
            ScreenAction::Save => {
                self.save_active(None);
                LoopCmd::None
            }
            ScreenAction::Back => {
                // FB3: Esc in the content returns focus to the sidebar (the
                // section stays loaded — a dirty section is still dirty).
                self.enter_nav_focus();
                LoopCmd::None
            }
            ScreenAction::RefreshDevices => {
                if let Active::Audio(s) = &self.active {
                    let backend = s.backend();
                    self.spawn_devices(backend);
                }
                LoopCmd::None
            }
            ScreenAction::ImportPlan(path) => {
                self.spawn_import_plan(path);
                LoopCmd::None
            }
            ScreenAction::ImportApply => {
                self.spawn_import_apply();
                LoopCmd::None
            }
            ScreenAction::Export { dest, include_auth } => {
                self.spawn_export(dest, include_auth);
                LoopCmd::None
            }
            ScreenAction::WizardProbeHealth => {
                self.spawn_wizard_health();
                LoopCmd::None
            }
            ScreenAction::WizardDetect => {
                self.spawn_wizard_detect();
                LoopCmd::None
            }
            ScreenAction::WizardGenConfigs(dacs) => {
                self.spawn_wizard_configs(dacs);
                LoopCmd::None
            }
            ScreenAction::WizardTestStart => {
                self.spawn_wizard_test(true);
                LoopCmd::None
            }
            ScreenAction::WizardTestPoll => {
                self.spawn_wizard_test(false);
                LoopCmd::None
            }
            ScreenAction::WizardAbandon => {
                self.overlay = Overlay::ConfirmAbandon;
                LoopCmd::None
            }
        }
    }

    // -------------------------- saves --------------------------

    fn save_active(&mut self, then_leave: Option<LeaveTarget>) {
        let (keys, network) = match &self.active {
            Active::Audio(s) => (s.save_keys(), None),
            Active::Playback(s) => (s.save_keys(), None),
            Active::QConnect(s) => (s.save_keys(), None),
            Active::Network(s) => match s.validated() {
                Ok(v) => (Vec::new(), Some(v)),
                Err(e) => {
                    self.overlay = Overlay::Result {
                        title: s::SAVE_TITLE.to_string(),
                        lines: vec![format!("cannot save: {e}")],
                    };
                    return;
                }
            },
            _ => return, // Account / Bundle / Menu never save
        };

        if keys.is_empty() && network.is_none() {
            // Nothing changed — just leave if that was the intent.
            if let Some(t) = then_leave {
                self.apply_leave(t);
            }
            return;
        }

        // The baseline is updated only on a SUCCESSFUL write (§4.2: a failed
        // store write leaves the screen dirty). Input is parked while busy, so
        // the staged form cannot change under the async save.
        self.leave_after_save = then_leave;
        self.busy = Some("saving…".to_string());
        let roots = self.roots.clone();
        let tx = self.tx.clone();
        let is_network = network.is_some();
        self.handle.spawn(async move {
            let (write_err, reinit) = if let Some((bind, port, token)) = network {
                (save_network(&roots, &bind, port, token.as_deref()), false)
            } else {
                write_keys(&roots, &keys)
            };
            let success = write_err.is_none();
            if let Some(err) = write_err {
                // Store write failed — do not touch the daemon; report the fault.
                let _ = tx.send(Msg::Saved {
                    lines: vec![err],
                    status: None,
                    reachable: true,
                    success: false,
                });
                return;
            }
            let (lines, status, reachable) = do_reload(&roots, is_network, reinit).await;
            let _ = tx.send(Msg::Saved {
                lines,
                status,
                reachable,
                success,
            });
        });
    }

    // -------------------------- immediate actions --------------------------

    fn spawn_devices(&mut self, backend: AudioBackendType) {
        let tx = self.tx.clone();
        self.handle.spawn_blocking(move || {
            let _ = tx.send(Msg::Devices(enumerate_devices(backend)));
        });
    }

    fn spawn_import_plan(&mut self, path: String) {
        self.busy = Some("reading bundle…".to_string());
        let roots = self.roots.clone();
        let tx = self.tx.clone();
        self.handle.spawn_blocking(move || {
            let _ = tx.send(Msg::ImportPlanned(plan_import(&roots, &path).map(Box::new)));
        });
    }

    fn spawn_import_apply(&mut self) {
        let ctx = match &self.active {
            Active::Bundle(s) => s.apply_context(),
            _ => None,
        };
        let Some((bundle, target, live, mut opts, choice, with_auth)) = ctx else {
            return;
        };
        self.busy = Some("applying import…".to_string());
        let roots = self.roots.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            opts.include_auth = with_auth;
            let msg = apply_import(&roots, bundle, target, live, opts, choice).await;
            let _ = tx.send(msg);
        });
    }

    fn spawn_export(&mut self, dest: String, include_auth: bool) {
        self.busy = Some("exporting…".to_string());
        let roots = self.roots.clone();
        let tx = self.tx.clone();
        self.handle.spawn_blocking(move || {
            let _ = tx.send(Msg::Exported(export_bundle(&roots, &dest, include_auth)));
        });
    }

    // -------------------------- wizard workers (FB4) --------------------------

    /// The heavy audio-stack health probe (shells out to systemctl/aplay/pw-dump)
    /// — never on the render thread.
    fn spawn_wizard_health(&mut self) {
        self.busy = Some(s::WIZ_HEALTH_CHECKING.to_string());
        let tx = self.tx.clone();
        self.handle.spawn_blocking(move || {
            let _ = tx.send(Msg::WizardHealth(qbz_audio::audio_stack_health()));
        });
    }

    /// Enumerate DAC candidates via the pw-dump-robust path (blocking).
    fn spawn_wizard_detect(&mut self) {
        self.busy = Some(s::WIZ_DETECTING.to_string());
        let tx = self.tx.clone();
        self.handle.spawn_blocking(move || {
            let _ = tx.send(Msg::WizardDacs(wizard_core::detect_blocking()));
        });
    }

    /// Re-probe rates + build the per-DAC config blocks (blocking).
    fn spawn_wizard_configs(&mut self, dacs: Vec<(String, String)>) {
        self.busy = Some(s::WIZ_GENERATING.to_string());
        let tx = self.tx.clone();
        self.handle.spawn_blocking(move || {
            let _ = tx.send(Msg::WizardConfigs(wizard_core::gen_configs_blocking(dacs)));
        });
    }

    /// Test read-back: optionally cold-start the daemon's OWN playback of the
    /// current queue, then sample the requested rate (`/api/status`) and the
    /// DAC's REAL negotiated rate (`/proc/asound` via qbz-audio::dac_probe). The
    /// requested-vs-negotiated pair is the bit-perfect proof (N6).
    fn spawn_wizard_test(&mut self, start_playback: bool) {
        self.busy = Some(if start_playback {
            "starting playback…".to_string()
        } else {
            "reading DAC…".to_string()
        });
        let roots = self.roots.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let client = ApiClient::new(None, &roots);
            let mut note = None;
            if start_playback {
                match client.post("/api/playback/play", json!({})).await {
                    Ok(_) => {
                        // Give the stream a moment to open on the DAC before the read.
                        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                    }
                    Err(CliError::Unreachable(_)) => {
                        note =
                            Some("daemon not reachable — start it, then try the test".to_string());
                    }
                    Err(_) => {
                        note = Some(
                            "nothing to play — queue a track or cast to the daemon first"
                                .to_string(),
                        );
                    }
                }
            }
            // Requested (what QBZ asked the daemon for), while playing.
            let requested = client.get("/api/status").await.ok().and_then(|st| {
                let sr = st.pointer("/audio/sample_rate").and_then(Value::as_u64)?;
                let bd = st
                    .pointer("/audio/bit_depth")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Some((sr as u32, bd as u32))
            });
            // Negotiated (the DAC's real hardware clock).
            let negotiated = qbz_audio::negotiated_active_rate();
            let _ = tx.send(Msg::WizardTest {
                requested,
                negotiated,
                note,
            });
        });
    }

    // -------------------------- worker results --------------------------

    pub fn drain_worker(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.on_msg(msg);
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Devices(result) => {
                if let Active::Audio(s) = &mut self.active {
                    s.set_devices(result);
                }
            }
            Msg::Saved {
                lines,
                status,
                reachable,
                success,
            } => {
                self.busy = None;
                self.reachable = reachable;
                if status.is_some() {
                    self.status = status;
                }
                self.overlay = Overlay::Result {
                    title: s::SAVE_TITLE.to_string(),
                    lines,
                };
                if success {
                    // §4.1: the staged form becomes the baseline (dirty clears).
                    match &mut self.active {
                        Active::Audio(sc) => sc.mark_saved(),
                        Active::Playback(sc) => sc.mark_saved(),
                        Active::QConnect(sc) => sc.mark_saved(),
                        Active::Network(sc) => sc.mark_saved(),
                        _ => {}
                    }
                    // Dirty-leave "Save" → leave once the save landed (§4.1).
                    if let Some(target) = self.leave_after_save.take() {
                        self.apply_leave(target);
                    }
                } else {
                    // §4.2: a failed write leaves the screen dirty; do not leave.
                    self.leave_after_save = None;
                }
            }
            Msg::ImportPlanned(result) => {
                self.busy = None;
                match result {
                    Ok(pending) => {
                        if let Active::Bundle(s) = &mut self.active {
                            s.set_plan(*pending);
                        }
                    }
                    Err(e) => {
                        self.overlay = Overlay::Result {
                            title: s::BUNDLE_TITLE.to_string(),
                            lines: e.lines().map(str::to_string).collect(),
                        };
                    }
                }
            }
            Msg::ImportApplied {
                lines,
                status,
                reachable,
            } => {
                self.busy = None;
                self.reachable = reachable;
                if status.is_some() {
                    self.status = status;
                }
                if let Active::Bundle(s) = &mut self.active {
                    s.clear_pending();
                }
                self.overlay = Overlay::Result {
                    title: s::BUNDLE_TITLE.to_string(),
                    lines,
                };
            }
            Msg::Exported(result) => {
                self.busy = None;
                let lines = match result {
                    Ok(lines) => lines,
                    Err(e) => e.lines().map(str::to_string).collect(),
                };
                self.overlay = Overlay::Result {
                    title: s::BUNDLE_TITLE.to_string(),
                    lines,
                };
            }
            Msg::WizardHealth(health) => {
                self.busy = None;
                if let Active::Wizard(w) = &mut self.active {
                    w.set_health(health);
                }
            }
            Msg::WizardDacs(dacs) => {
                self.busy = None;
                if let Active::Wizard(w) = &mut self.active {
                    w.set_candidates(dacs);
                }
            }
            Msg::WizardConfigs(configs) => {
                self.busy = None;
                if let Active::Wizard(w) = &mut self.active {
                    w.set_configs(configs);
                }
            }
            Msg::WizardTest {
                requested,
                negotiated,
                note,
            } => {
                self.busy = None;
                if let Active::Wizard(w) = &mut self.active {
                    w.set_test_result(requested, negotiated, note);
                }
            }
        }
    }

    // -------------------------- render --------------------------

    pub fn draw(&self, f: &mut Frame) {
        let area = f.area();
        if area.width < 80 || area.height < 24 {
            let msg = s::too_small(area.width, area.height);
            f.render_widget(Paragraph::new(msg), area);
            return;
        }

        // FB3 frames layout: header · breadcrumb · [sidebar | content] · footer ·
        // help. The 80×24 floor budget is 1+1 (top chrome) + 1+1 (bottom chrome)
        // + a Min(3) body, so the content frame keeps ≥ 18 usable inner rows.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Length(1), // breadcrumb
                Constraint::Min(3),    // body (sidebar + content)
                Constraint::Length(1), // footer (daemon state)
                Constraint::Length(1), // help bar
            ])
            .split(area);
        self.draw_header(f, rows[0]);
        self.draw_breadcrumb(f, rows[1]);

        let sidebar_w = widgets::sidebar_width(area.width);
        let wide = widgets::sidebar_is_wide(area.width);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_w), Constraint::Min(0)])
            .split(rows[2]);
        self.draw_sidebar(f, body[0], wide);

        // Content frame: accent border when the content owns focus, dim otherwise
        // (its title is gone — the breadcrumb names the section now).
        let border = if self.focus == Focus::Content {
            theme::accent()
        } else {
            theme::dim()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border);
        let inner = block.inner(body[1]);
        f.render_widget(block, body[1]);

        let ctx = DrawCtx {
            status: self.status.as_ref(),
        };
        match &self.active {
            Active::Audio(sc) => sc.draw(f, inner, &ctx),
            Active::Playback(sc) => sc.draw(f, inner, &ctx),
            Active::QConnect(sc) => sc.draw(f, inner, &ctx),
            Active::Network(sc) => sc.draw(f, inner, &ctx),
            Active::Bundle(sc) => sc.draw(f, inner, &ctx),
            Active::Wizard(sc) => sc.draw(f, inner, &ctx),
        }

        self.draw_footer(f, rows[3]);
        widgets::help_bar(f, rows[4], self.help_text());

        // Overlays (help / result / dirty-leave / busy) cover the WHOLE screen —
        // they are the third navigation level and sit above the frames.
        match &self.overlay {
            Overlay::Help => widgets::panel(
                f,
                area,
                s::HELP_TITLE,
                s::HELP_OVERLAY
                    .lines()
                    .map(|l| Line::from(l.to_string()))
                    .collect(),
                0,
            ),
            Overlay::Result { title, lines } => {
                let body = lines.join("\n");
                widgets::modal(f, area, title, &body, s::RESULT_HINT);
            }
            Overlay::DirtyLeave { .. } => {
                widgets::modal(f, area, s::DIRTY_TITLE, s::DIRTY_BODY, s::DIRTY_HINT);
            }
            Overlay::ConfirmAbandon => {
                widgets::modal(
                    f,
                    area,
                    s::WIZ_ABANDON_TITLE,
                    s::WIZ_ABANDON_BODY,
                    s::WIZ_ABANDON_HINT,
                );
            }
            Overlay::None => {}
        }

        if let Some(label) = &self.busy {
            widgets::busy_overlay(f, area, label, self.busy_tick);
        }
    }

    /// Header row: `QBZ Daemon Setup` (accent-bold, left) · `qbzd <version>`
    /// (dim, right). One row, always visible.
    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let title = s::APP_TITLE;
        let version = format!("qbzd {}", env!("CARGO_PKG_VERSION"));
        let used = 1 + title.chars().count() + version.chars().count() + 1;
        let pad = (area.width as usize).saturating_sub(used);
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_string(), theme::accent_bold()),
            Span::raw(" ".repeat(pad)),
            Span::styled(version, theme::dim()),
            Span::raw(" "),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    /// Breadcrumb row (max 2 levels): dim `Setup ›` prefix + accent current node;
    /// while a field is edited it becomes `<Section> › <Field>`.
    fn draw_breadcrumb(&self, f: &mut Frame, area: Rect) {
        let section = section_title(self.active_section);
        let (prefix, current) = breadcrumb_nodes(section, self.active_editing_label());
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(prefix.to_string(), theme::dim()),
            Span::styled(" › ".to_string(), theme::dim()),
            Span::styled(current.to_string(), theme::accent()),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    /// Persistent left navigation: the sections by name. The active one gets
    /// `▸` + accent; a dirty active section gets a warn `*`. When the nav owns
    /// focus, the highlighted NAME row reverses (serial-safe) and the border
    /// accents. In the wide tier (FB5) the labels spell out (`Import / Export`)
    /// and each name carries a dim static summary line beneath it.
    fn draw_sidebar(&self, f: &mut Frame, area: Rect, wide: bool) {
        let border = if self.focus == Focus::Nav {
            theme::accent()
        } else {
            theme::dim()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let active_dirty = self.active_is_dirty();
        let mut lines: Vec<Line> = Vec::new();
        for (i, screen) in SCREENS.iter().enumerate() {
            let is_active = *screen == self.active_section;
            let dirty = sidebar_dirty_marker(*screen, self.active_section, active_dirty);
            let highlighted = self.focus == Focus::Nav && i == self.nav_cursor;
            let label = if wide {
                s::SIDEBAR_LABELS_WIDE[i]
            } else {
                s::SIDEBAR_LABELS[i]
            };
            let marker = if is_active { "▸ " } else { "  " };

            if highlighted {
                // Full-width reverse bar (accent-reversed) — reads on monochrome
                // and serial. Padded to the inner width so the bar spans the row.
                let dirty_str = if dirty { " *" } else { "" };
                let core = format!("{marker}{label}{dirty_str}");
                let padded = format!("{core:<width$}");
                lines.push(Line::from(Span::styled(padded, theme::selection())));
            } else {
                let mut spans = vec![
                    Span::styled(
                        marker.to_string(),
                        if is_active {
                            theme::accent()
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(
                        label.to_string(),
                        if is_active {
                            theme::accent_bold()
                        } else {
                            Style::default()
                        },
                    ),
                ];
                if dirty {
                    spans.push(Span::styled(" *".to_string(), theme::warn()));
                }
                lines.push(Line::from(spans));
            }
            // Wide tier: a dim static summary under each name (never live state).
            if wide {
                lines.push(Line::from(Span::styled(
                    format!("    {}", s::SIDEBAR_SUMMARIES[i]),
                    theme::dim(),
                )));
            }
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The daemon-state footer, color-coded via `footer_state`. Never color
    /// alone — every state spells itself out.
    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let playing = self.status.as_ref().and_then(playing_extra);
        let (text, style) = footer_state(self.reachable, playing);
        f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
    }

    fn help_text(&self) -> &'static str {
        match self.focus {
            Focus::Nav => s::HELP_NAV,
            Focus::Content => match &self.active {
                Active::Audio(sc) => {
                    if sc.is_dirty() {
                        s::HELP_AUDIO_DIRTY
                    } else {
                        s::HELP_AUDIO_CLEAN
                    }
                }
                Active::Wizard(sc) => sc.help_text(),
                _ => {
                    if self.active_is_dirty() {
                        s::HELP_CONTENT_DIRTY
                    } else {
                        s::HELP_CONTENT_CLEAN
                    }
                }
            },
        }
    }
}

// ============================ worker functions ============================

async fn fetch_status(roots: ProfileRoots) -> Option<Value> {
    let client = ApiClient::new(None, &roots);
    client.get("/api/status").await.ok()
}

fn enumerate_devices(backend: AudioBackendType) -> Result<Vec<AudioDevice>, String> {
    BackendManager::create_backend(backend)
        .and_then(|b| b.enumerate_devices())
        .map_err(|e| e.to_string())
}

fn load_audio(roots: &ProfileRoots) -> AudioSettings {
    AudioSettingsStore::new_at(&roots.data)
        .and_then(|s| s.get_settings())
        .unwrap_or_default()
}

/// Persist changed keys through T11's `write_one`. Returns `(Some(error_line),
/// reinit)` — a mid-set failure names the key; `reinit` is true when any written
/// key was Reinit-class (§4.3 client-side classification).
fn write_keys(roots: &ProfileRoots, keys: &[(String, String)]) -> (Option<String>, bool) {
    let mut reinit = false;
    for (k, v) in keys {
        match crate::cli::settings::write_one(roots, k, v) {
            Ok(class) => {
                if class == crate::cli::settings::ApplyClass::Reinit {
                    reinit = true;
                }
            }
            Err(e) => {
                // The TUI only displays the message — it doesn't need the
                // Usage/Io exit-code split `settings set` maps to (see
                // `cli::settings::SetError`).
                return (
                    Some(format!("failed to save {k}: {}", e.to_string().trim())),
                    reinit,
                );
            }
        }
    }
    (None, reinit)
}

fn save_network(
    roots: &ProfileRoots,
    bind: &str,
    port: u16,
    token: Option<&str>,
) -> Option<String> {
    let path = roots.config.join("qbzd.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    match network_screen::rewrite_toml(&existing, bind, port, token) {
        Ok(text) => match std::fs::write(&path, text) {
            Ok(()) => None,
            Err(e) => Some(format!("failed to write qbzd.toml: {e}")),
        },
        Err(e) => Some(format!("failed to rewrite qbzd.toml: {e}")),
    }
}

/// POST /api/settings/reload and compose the §4.3 result. Returns
/// `(lines, status_body, reachable)`.
async fn do_reload(
    roots: &ProfileRoots,
    is_network: bool,
    reinit: bool,
) -> (Vec<String>, Option<Value>, bool) {
    let client = ApiClient::new(None, roots);
    match client.post("/api/settings/reload", json!({})).await {
        Ok(body) => {
            let lines = if is_network {
                vec!["saved.".to_string(), s::NETWORK_RESTART.to_string()]
            } else {
                let mut line = "saved · daemon reloaded".to_string();
                if reinit {
                    line.push_str(" (output device reinitialized");
                    if let Some(extra) = playing_extra(&body) {
                        line.push_str(&format!(" · {extra}"));
                    }
                    line.push(')');
                }
                vec![line]
            };
            (lines, Some(body), true)
        }
        Err(CliError::Unreachable(_)) => {
            let lines = if is_network {
                vec!["saved.".to_string(), s::APPLIES_ON_START.to_string()]
            } else {
                vec![s::SAVED_DISK_ONLY.to_string()]
            };
            (lines, None, false)
        }
        Err(_) => (vec![s::RELOAD_REFUSED.to_string()], None, true),
    }
}

fn plan_import(roots: &ProfileRoots, path: &str) -> Result<PendingImport, String> {
    let text = std::fs::read_to_string(expand_tilde(path))
        .map_err(|e| format!("cannot read bundle: {e}"))?;
    let bundle = Bundle::parse(&text).map_err(|e| e.to_string())?;
    let (live, backend, devices) = build_live(&bundle);
    let target = ProfilePaths {
        config_root: roots.config.clone(),
        data_root: roots.data.clone(),
    };
    let opts = ImportOptions {
        include_auth: false,
        remap: Vec::new(),
        non_tty: false,
    };
    let plan = bundle::plan(&bundle, &target, &opts, &live).map_err(|e| e.to_string())?;
    let has_auth = bundle
        .domains
        .get("auth")
        .and_then(Value::as_object)
        .and_then(|a| a.get("user_auth_token"))
        .and_then(Value::as_str)
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    Ok(PendingImport {
        bundle,
        plan,
        live,
        opts,
        target,
        backend,
        devices,
        device_choice: None,
        has_auth,
        apply_with_auth: false,
    })
}

async fn apply_import(
    roots: &ProfileRoots,
    bundle: Bundle,
    target: ProfilePaths,
    live: LiveSystem,
    opts: ImportOptions,
    choice: Option<bundle::DeviceChoice>,
) -> Msg {
    let plan = match &choice {
        Some(c) => bundle::replan_with_device(&bundle, &target, &opts, &live, c.clone()),
        None => bundle::plan(&bundle, &target, &opts, &live),
    };
    let plan = match plan {
        Ok(p) => p,
        Err(e) => {
            return Msg::ImportApplied {
                lines: vec![e.to_string()],
                status: None,
                reachable: false,
            }
        }
    };

    let uid: Option<u64> = None;
    if let Err(e) = bundle::apply(&plan, &target, uid) {
        return Msg::ImportApplied {
            lines: vec![format!("import only partially applied: {e}")],
            status: None,
            reachable: false,
        };
    }

    let (mut lines, status, reachable) =
        do_reload(roots, false, plan.routing_critical_changed).await;
    let mut out = vec![s::b_import_done(
        plan.applied.len(),
        plan.adapted.len(),
        plan.skipped.len(),
    )];
    out.append(&mut lines);
    if uid.is_some() {
        out.push("logged in with the bundled account".to_string());
    }
    Msg::ImportApplied {
        lines: out,
        status,
        reachable,
    }
}

fn export_bundle(
    roots: &ProfileRoots,
    dest: &str,
    include_auth: bool,
) -> Result<Vec<String>, String> {
    let source = ExportSource::Daemon(ProfilePaths {
        config_root: roots.config.clone(),
        data_root: roots.data.clone(),
    });
    let b = bundle::export(source, &ExportOptions { include_auth }).map_err(|e| e.to_string())?;
    let path = expand_tilde(dest);
    bundle::write_bundle_file(&path, &b).map_err(|e| e.to_string())?;

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(dest)
        .to_string();
    let mut lines = vec![s::b_export_success(&name)];
    if b.contains_secrets() {
        lines.push(
            "this file contains your Qobuz token — 0600, move it privately, delete after import"
                .to_string(),
        );
    }
    if desktop_profile_present() {
        for l in s::B_DESKTOP_HINT.lines() {
            lines.push(l.to_string());
        }
    }
    Ok(lines)
}

// ============================ helpers ============================

/// The bundle's target backend + a live enumeration for the re-pick picker
/// (mirrors cli/settings.rs `build_live_system`).
fn build_live(bundle: &Bundle) -> (LiveSystem, AudioBackendType, Vec<AudioDevice>) {
    let backends: Vec<String> = BackendManager::available_backends()
        .into_iter()
        .filter_map(|b| {
            serde_json::to_value(b)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
        })
        .collect();
    let wanted: Option<AudioBackendType> = bundle
        .domains
        .get("audio")
        .and_then(|a| a.get("backend_type"))
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let backend = wanted.unwrap_or(AudioBackendType::SystemDefault);
    let devices = enumerate_devices(backend).unwrap_or_default();
    let live_devices: Vec<(String, String)> = devices
        .iter()
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect();
    (
        LiveSystem {
            backends,
            devices: live_devices,
        },
        backend,
        devices,
    )
}

/// Pure footer mapping (tested below). Three states, each spelled out in text —
/// the tone only reinforces it:
/// - unreachable → dim `daemon: not reachable`;
/// - reachable but not signed in → warn `daemon: running · not signed in`
///   (a deliberate FB2 addition over the base footer: an operator-visible
///   needs-auth cue, owner veto at the smoke);
/// - running + signed in → ok, with the optional `playing …` tail.
fn footer_state(reachable: bool, playing: Option<String>) -> (String, ratatui::style::Style) {
    if !reachable {
        (format!(" {}", s::FOOTER_UNREACHABLE), theme::dim())
    } else {
        let text = match playing {
            Some(e) => format!(" {} · {e}", s::FOOTER_RUNNING),
            None => format!(" {}", s::FOOTER_RUNNING),
        };
        (text, theme::ok())
    }
}

/// A "playing 192000 Hz / 24 bit" tail from a status body (§4.3), if playing.
fn playing_extra(status: &Value) -> Option<String> {
    let state = status
        .pointer("/playback/state")
        .and_then(Value::as_str)
        .unwrap_or("");
    if state != "playing" {
        return None;
    }
    let sr = status
        .pointer("/audio/sample_rate")
        .and_then(Value::as_u64)?;
    let bd = status.pointer("/audio/bit_depth").and_then(Value::as_u64)?;
    Some(format!("playing {sr} Hz / {bd} bit"))
}

fn desktop_profile_present() -> bool {
    dirs::data_dir()
        .map(|d| d.join("qbz").exists())
        .unwrap_or(false)
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use ratatui::Terminal;

    // ---- landing (FB3): always Account; focus depends on the credential file ----

    // ---- breadcrumb composition (max 2 levels) ----

    #[test]
    fn breadcrumb_is_setup_then_section_when_not_editing() {
        assert_eq!(breadcrumb_nodes("Audio", None), ("Setup", "Audio"));
    }

    #[test]
    fn breadcrumb_is_section_then_field_when_editing() {
        assert_eq!(
            breadcrumb_nodes("Audio", Some("Backend")),
            ("Audio", "Backend")
        );
    }

    // ---- sidebar dirty marker: only the active section can show `*` ----

    #[test]
    fn dirty_marker_only_on_the_active_dirty_section() {
        assert!(sidebar_dirty_marker(Screen::Audio, Screen::Audio, true));
        assert!(!sidebar_dirty_marker(Screen::Audio, Screen::Audio, false));
        // A non-active section is clean by construction (leave is guarded).
        assert!(!sidebar_dirty_marker(Screen::Playback, Screen::Audio, true));
    }

    // ---- focus-transition table (Tab / Esc / Enter / arrows / number keys) ----

    #[test]
    fn nav_focus_key_table() {
        use NavIntent::*;
        let n = |code| classify_key(Focus::Nav, code, false, false);
        assert_eq!(n(KeyCode::Up), MoveCursor(-1));
        assert_eq!(n(KeyCode::Char('k')), MoveCursor(-1));
        assert_eq!(n(KeyCode::Down), MoveCursor(1));
        assert_eq!(n(KeyCode::Char('j')), MoveCursor(1));
        assert_eq!(n(KeyCode::Enter), ActivateCursor);
        assert_eq!(n(KeyCode::Right), ActivateCursor);
        assert_eq!(n(KeyCode::Tab), ActivateCursor);
        assert_eq!(n(KeyCode::Esc), Quit);
        assert_eq!(n(KeyCode::Char('q')), Quit);
        assert_eq!(n(KeyCode::Char('?')), Help);
        assert_eq!(n(KeyCode::Left), None); // no-op at the left edge
    }

    #[test]
    fn content_focus_key_table() {
        use NavIntent::*;
        let c = |code| classify_key(Focus::Content, code, false, false);
        // Tab and (un-consumed) ← walk back to the sidebar.
        assert_eq!(c(KeyCode::Tab), FocusNav);
        assert_eq!(c(KeyCode::Left), FocusNav);
        // These belong to the screen (Esc returns Back → the App re-focuses nav).
        assert_eq!(c(KeyCode::Esc), ToScreen);
        assert_eq!(c(KeyCode::Up), ToScreen);
        assert_eq!(c(KeyCode::Enter), ToScreen);
        assert_eq!(c(KeyCode::Char('s')), ToScreen);
        assert_eq!(c(KeyCode::Right), ToScreen);
        // Global chrome.
        assert_eq!(c(KeyCode::Char('q')), Quit);
        assert_eq!(c(KeyCode::Char('?')), Help);
    }

    #[test]
    fn left_is_consumed_by_a_horizontal_field() {
        // Audio's Buffer slider claims ← — it must NOT drop focus to the nav.
        assert_eq!(
            classify_key(Focus::Content, KeyCode::Left, false, true),
            NavIntent::ToScreen
        );
    }

    #[test]
    fn wizard_welcome_left_drops_focus_to_nav_like_every_other_section() {
        // A fresh Wizard starts on Welcome, where nothing claims ← — it must
        // behave exactly like every non-Wizard section: ← walks focus back
        // to the sidebar instead of being swallowed by the wizard's own
        // (no-op at Welcome) step-back handling. This only exercises the
        // FocusNav path (no screen dispatch), so it never touches the
        // wizard's async health-probe worker.
        let mut app = bare_app(Screen::Wizard, Focus::Content);
        assert!(!app.content_uses_horizontal());
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Nav);
    }

    #[test]
    fn number_keys_jump_from_any_focus_but_not_while_editing() {
        // 1-7 jump from nav and from content (not editing).
        assert_eq!(
            classify_key(Focus::Nav, KeyCode::Char('3'), false, false),
            NavIntent::JumpSection(2)
        );
        assert_eq!(
            classify_key(Focus::Content, KeyCode::Char('1'), false, false),
            NavIntent::JumpSection(0)
        );
        assert_eq!(
            classify_key(Focus::Content, KeyCode::Char('6'), false, false),
            NavIntent::JumpSection(5)
        );
        // 7 reaches the FB4 Wizard section (the seventh).
        assert_eq!(
            classify_key(Focus::Content, KeyCode::Char('7'), false, false),
            NavIntent::JumpSection(6)
        );
        // While a field editor is open, digits are typed into it, not swallowed.
        assert_eq!(
            classify_key(Focus::Content, KeyCode::Char('5'), true, false),
            NavIntent::ToScreen
        );
        // ...and so is every other key while editing.
        assert_eq!(
            classify_key(Focus::Content, KeyCode::Tab, true, false),
            NavIntent::ToScreen
        );
        assert_eq!(
            classify_key(Focus::Content, KeyCode::Esc, true, false),
            NavIntent::ToScreen
        );
    }

    // ---- 80×24 floor: every section renders inside the frames shell ----

    fn bare_app(section: Screen, focus: Focus) -> App {
        let (tx, rx) = std::sync::mpsc::channel();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let active = match section {
            Screen::Audio => Active::Audio(AudioState::new(&AudioSettings::default())),
            Screen::Playback => Active::Playback(PlaybackState::new(
                "hires_plus",
                true,
                &AudioSettings::default(),
                &qbz_app::settings::playback::PlaybackPreferences::default(),
            )),
            Screen::QConnect => Active::QConnect(QConnectState::new(false, None, None)),
            Screen::Network => {
                Active::Network(NetworkState::new(&QbzdConfig::default(), Vec::new()))
            }
            Screen::Bundle => Active::Bundle(BundleState::new(false)),
            Screen::Wizard => Active::Wizard(WizardState::new()),
        };
        App {
            roots: ProfileRoots {
                config: PathBuf::from("/nonexistent"),
                data: PathBuf::from("/nonexistent"),
                cache: PathBuf::from("/nonexistent"),
            },
            handle: rt.handle().clone(),
            tx,
            rx,
            active,
            active_section: section,
            nav_cursor: section_index(section),
            focus,
            status: None,
            reachable: false,
            overlay: Overlay::None,
            busy: None,
            busy_tick: 0,
            should_quit: false,
            leave_after_save: None,
        }
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn every_section_fits_the_80x24_floor() {
        for screen in SCREENS {
            for focus in [Focus::Nav, Focus::Content] {
                let app = bare_app(screen, focus);
                let out = render(&app, 80, 24);
                // The shell rendered (not the too-small guard).
                assert!(
                    out.contains("μqbzd Setup"),
                    "header missing for {screen:?}/{focus:?}"
                );
                assert!(
                    !out.contains("terminal too small"),
                    "80x24 must not trip the resize guard for {screen:?}"
                );
                // The sidebar and version chrome are present.
                // "Audio" rather than "Account": the Account section went with
                // the login path, and Audio is now the first sidebar row.
                assert!(out.contains("Audio"), "sidebar missing for {screen:?}");
                let version = format!("qbzd {}", env!("CARGO_PKG_VERSION"));
                assert!(
                    out.contains(&version),
                    "version chrome missing for {screen:?}"
                );
            }
        }
    }

    #[test]
    fn resize_guard_still_fires_below_the_floor() {
        let app = bare_app(Screen::Audio, Focus::Content);
        let out = render(&app, 79, 24);
        assert!(out.contains("terminal too small"));
    }

    // ---- 120×30 wide: the roomy sidebar tier + every section still renders (FB5) ----

    #[test]
    fn every_section_fits_the_120x30_wide() {
        for screen in SCREENS {
            for focus in [Focus::Nav, Focus::Content] {
                let app = bare_app(screen, focus);
                let out = render(&app, 120, 30);
                assert!(
                    out.contains("μqbzd Setup"),
                    "header missing {screen:?}/{focus:?}"
                );
                assert!(
                    !out.contains("terminal too small"),
                    "120x30 must not trip the resize guard for {screen:?}"
                );
                // Wide tier: labels spell out and each name carries a dim summary.
                assert!(
                    out.contains("Import / Export"),
                    "wide sidebar label missing {screen:?}"
                );
                assert!(
                    out.contains("output · bit-perfect"),
                    "wide sidebar summary missing {screen:?}"
                );
            }
        }
    }

    #[test]
    fn sidebar_is_compact_at_the_floor_and_wide_above_100() {
        // At the 80-col floor the compact label is used, not the spelled-out one.
        let floor = render(&bare_app(Screen::Audio, Focus::Nav), 80, 24);
        assert!(floor.contains("Import/Exp"), "compact label at the floor");
        assert!(
            !floor.contains("Import / Export"),
            "no wide label at the floor"
        );
        // The 28-col sidebar is at least double the 14-col compact one.
        assert!(widgets::sidebar_width(120) >= 2 * widgets::sidebar_width(80));
    }

    /// The footer now has two states, not three: the "needs auth" cue went with
    /// the account path.
    #[test]
    fn footer_state_maps_the_two_daemon_states() {
        let (text, style) = footer_state(false, None);
        assert_eq!(text, format!(" {}", s::FOOTER_UNREACHABLE));
        assert_eq!(style, theme::dim());

        let (text, style) = footer_state(true, None);
        assert_eq!(text, format!(" {}", s::FOOTER_RUNNING));
        assert_eq!(style, theme::ok());

        let (text, _) = footer_state(true, Some("playing 96000 Hz / 24 bit".into()));
        assert_eq!(
            text,
            format!(" {} · playing 96000 Hz / 24 bit", s::FOOTER_RUNNING)
        );
    }
}
