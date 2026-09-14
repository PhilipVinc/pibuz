// crates/pibuz/src/tui/screens/playback.rs — the Playback screen (03 §3.3).
//
// Reads three stores at entry (daemon_prefs.streaming_quality, AudioSettings
// quality rows, PlaybackPreferences) and writes back through the App's
// write_one path. One spec subtlety, pure + tested:
//   - `infinite` autoplay (P1 radio) renders read-only until toggled (§3.3.1).
//
// The Quality group used to carry four more rows — a device-rate limiter, its
// max-rate select, an allow-fallback toggle and a retry-on-failure select, plus
// the `ask` rendering rule that hung off the last of them. They are gone with
// the keys behind them (see `cli::settings::KEY_TABLE`): the daemon reads none
// of the four, so every one of them was a control the operator could move
// without anything changing.

use qbz_app::settings::playback::{AutoplayMode, PlaybackPreferences};
use qbz_audio::settings::AudioSettings;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;

use crate::tui::app::{DrawCtx, ScreenAction};
use crate::tui::strings as s;
use crate::tui::widgets::{self, SelectOutcome, SelectPopup};

#[derive(Debug, Clone, PartialEq)]
pub struct StagedPlayback {
    pub quality: String,       // playback.quality
    pub autoplay: String,      // playback.autoplay
    pub gapless: bool,         // audio.gapless_enabled
    pub restore_session: bool, // playback.persist_session
    pub resume_position: bool, // playback.resume_playback_position
    pub mpris: bool,           // playback.mpris (applies on restart)
    /// Read-only, from the audio store — drives the Gapless disabled reason.
    pub streaming_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PField {
    Quality,
    Continue,
    Gapless,
    Restore,
    Resume,
    Mpris,
}

/// `(shown, enabled, reason)` per §3.3.
pub fn row_state(field: PField, p: &StagedPlayback) -> (bool, bool, Option<&'static str>) {
    use PField::*;
    match field {
        Gapless => (
            true,
            !p.streaming_only,
            if p.streaming_only {
                Some(s::R_STREAMING_ONLY_ON)
            } else {
                None
            },
        ),
        Resume => (
            true,
            p.restore_session,
            if p.restore_session {
                None
            } else {
                Some(s::R_RESTORE_OFF)
            },
        ),
        _ => (true, true, None),
    }
}

pub fn visible_fields(p: &StagedPlayback) -> Vec<PField> {
    use PField::*;
    [Quality, Continue, Gapless, Restore, Resume, Mpris]
        .into_iter()
        .filter(|f| row_state(*f, p).0)
        .collect()
}

enum Editor {
    Quality(SelectPopup),
}

pub struct PlaybackState {
    baseline: StagedPlayback,
    staged: StagedPlayback,
    focus: usize,
    editor: Option<Editor>,
}

impl PlaybackState {
    pub fn new(
        quality: &str,
        mpris: bool,
        audio: &AudioSettings,
        prefs: &PlaybackPreferences,
    ) -> Self {
        let staged = StagedPlayback {
            quality: quality.to_string(),
            autoplay: autoplay_value(prefs.autoplay_mode).to_string(),
            gapless: audio.gapless_enabled,
            restore_session: prefs.persist_session,
            resume_position: prefs.resume_playback_position,
            mpris,
            streaming_only: audio.streaming_only,
        };
        Self {
            baseline: staged.clone(),
            staged,
            focus: 0,
            editor: None,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.staged != self.baseline
    }
    pub fn is_editing(&self) -> bool {
        self.editor.is_some()
    }
    pub fn mark_saved(&mut self) {
        self.baseline = self.staged.clone();
    }

    /// The breadcrumb's level-2 node when a picker is open.
    pub fn editing_label(&self) -> Option<&'static str> {
        match &self.editor {
            Some(Editor::Quality(_)) => Some(s::P_QUALITY),
            None => None,
        }
    }

    /// Changed dotted keys for write_one.
    pub fn save_keys(&self) -> Vec<(String, String)> {
        let b = &self.baseline;
        let a = &self.staged;
        let mut out = Vec::new();
        if a.quality != b.quality {
            out.push(("playback.quality".to_string(), a.quality.clone()));
        }
        if a.autoplay != b.autoplay {
            out.push(("playback.autoplay".to_string(), a.autoplay.clone()));
        }
        if a.gapless != b.gapless {
            out.push(("audio.gapless_enabled".to_string(), a.gapless.to_string()));
        }
        if a.restore_session != b.restore_session {
            out.push((
                "playback.persist_session".to_string(),
                a.restore_session.to_string(),
            ));
        }
        if a.resume_position != b.resume_position {
            out.push((
                "playback.resume_playback_position".to_string(),
                a.resume_position.to_string(),
            ));
        }
        if a.mpris != b.mpris {
            out.push(("playback.mpris".to_string(), a.mpris.to_string()));
        }
        out
    }

    // -------------------------- input --------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> ScreenAction {
        if self.editor.is_some() {
            return self.handle_editor_key(key);
        }
        let fields = visible_fields(&self.staged);
        if self.focus >= fields.len() {
            self.focus = fields.len().saturating_sub(1);
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                if self.focus == 0 {
                    self.focus = fields.len().saturating_sub(1);
                } else {
                    self.focus -= 1;
                }
                ScreenAction::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                if !fields.is_empty() {
                    self.focus = (self.focus + 1) % fields.len();
                }
                ScreenAction::Consumed
            }
            KeyCode::Char('s') => ScreenAction::Save,
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(f) = fields.get(self.focus).copied() {
                    self.activate(f);
                }
                ScreenAction::Consumed
            }
            KeyCode::Esc => ScreenAction::Back,
            _ => ScreenAction::Consumed,
        }
    }

    fn activate(&mut self, field: PField) {
        let (_, enabled, _) = row_state(field, &self.staged);
        if !enabled {
            return;
        }
        match field {
            PField::Quality => {
                let opts = vec![
                    s::Q_MP3.to_string(),
                    s::Q_CD.to_string(),
                    s::Q_HIRES.to_string(),
                    s::Q_HIRES_PLUS.to_string(),
                ];
                let sel = match self.staged.quality.as_str() {
                    "mp3" => 0,
                    "cd" => 1,
                    "hires" => 2,
                    _ => 3,
                };
                self.editor = Some(Editor::Quality(SelectPopup::new(
                    s::P_QUALITY,
                    opts,
                    sel,
                    false,
                )));
            }
            PField::Continue => {
                // §3.3.1: infinite (radio) is preserved until toggled; the first
                // toggle from infinite lands on off (track_only).
                self.staged.autoplay = match self.staged.autoplay.as_str() {
                    "continue" => "track_only",
                    _ => "continue", // track_only or infinite → continue
                }
                .to_string();
            }
            PField::Gapless => self.staged.gapless ^= true,
            PField::Restore => self.staged.restore_session ^= true,
            PField::Resume => self.staged.resume_position ^= true,
            PField::Mpris => self.staged.mpris ^= true,
        }
    }

    fn handle_editor_key(&mut self, key: KeyEvent) -> ScreenAction {
        let editor = self.editor.take().unwrap();
        match editor {
            Editor::Quality(mut p) => match p.handle_key(key) {
                SelectOutcome::Chosen(i) => {
                    self.staged.quality = ["mp3", "cd", "hires", "hires_plus"]
                        .get(i)
                        .copied()
                        .unwrap_or("hires_plus")
                        .to_string();
                    ScreenAction::Consumed
                }
                SelectOutcome::Cancelled => ScreenAction::Consumed,
                SelectOutcome::Pending => {
                    self.editor = Some(Editor::Quality(p));
                    ScreenAction::Consumed
                }
            },
        }
    }

    // -------------------------- render --------------------------

    pub fn draw(&self, f: &mut Frame, area: Rect, _ctx: &DrawCtx) {
        let width = area.width.saturating_sub(2); // section inner width
        let fields = visible_fields(&self.staged);
        let focused_field = fields.get(self.focus).copied();
        let active = |members: &[PField]| {
            focused_field
                .map(|ff| members.contains(&ff))
                .unwrap_or(false)
        };
        // ONE control column for the whole screen (owner's "misma área de columna").
        let labels: Vec<&str> = fields.iter().map(|f| self.field_display(*f).0).collect();
        let ctrl_col = widgets::control_column(&labels, width);

        use PField::*;
        let mut secs: Vec<widgets::Section> = Vec::new();
        let mut anchor: Option<widgets::FocusAnchor> = None;

        let quality: &[PField] = &[Quality];
        let (q_lines, q_a) = self.group_block(&fields, quality, focused_field, ctrl_col, width);
        if !q_lines.is_empty() {
            widgets::push_section(
                &mut secs,
                &mut anchor,
                s::PLAYBACK_GROUP_QUALITY,
                active(quality),
                q_lines,
                q_a,
            );
        }

        let behavior: &[PField] = &[Continue, Gapless];
        let (b_lines, b_a) = self.group_block(&fields, behavior, focused_field, ctrl_col, width);
        if !b_lines.is_empty() {
            widgets::push_section(
                &mut secs,
                &mut anchor,
                s::PLAYBACK_GROUP_BEHAVIOR,
                active(behavior),
                b_lines,
                b_a,
            );
        }

        let session: &[PField] = &[Restore, Resume];
        let (sess_lines, sess_a) =
            self.group_block(&fields, session, focused_field, ctrl_col, width);
        if !sess_lines.is_empty() {
            widgets::push_section(
                &mut secs,
                &mut anchor,
                s::PLAYBACK_GROUP_SESSION,
                active(session),
                sess_lines,
                sess_a,
            );
        }

        let controls: &[PField] = &[Mpris];
        let (ctl_lines, ctl_a) =
            self.group_block(&fields, controls, focused_field, ctrl_col, width);
        if !ctl_lines.is_empty() {
            widgets::push_section(
                &mut secs,
                &mut anchor,
                s::PLAYBACK_GROUP_CONTROLS,
                active(controls),
                ctl_lines,
                ctl_a,
            );
        }

        widgets::sections_scroll(f, area, &secs, anchor);

        if let Some(Editor::Quality(p)) = &self.editor {
            p.draw(f, area);
        }
    }

    /// The field blocks of one group, in declared order (skipping hidden fields).
    /// Returns the flattened lines plus, when the focused field is in this group,
    /// its (first-line, height) inside the group for follow-focus scrolling.
    fn group_block(
        &self,
        fields: &[PField],
        members: &[PField],
        focused_field: Option<PField>,
        ctrl_col: u16,
        width: u16,
    ) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
        let mut lines = Vec::new();
        let mut within = None;
        for gf in members {
            if let Some(pos) = fields.iter().position(|x| x == gf) {
                let start = lines.len() as u16;
                let block = self.field_block(*gf, pos, ctrl_col, width);
                if focused_field == Some(*gf) {
                    within = Some((start, block.len() as u16));
                }
                lines.extend(block);
            }
        }
        (lines, within)
    }

    fn field_block(
        &self,
        field: PField,
        focus_pos: usize,
        ctrl_col: u16,
        width: u16,
    ) -> Vec<Line<'static>> {
        let (_, enabled, reason) = row_state(field, &self.staged);
        let focused = focus_pos == self.focus && self.editor.is_none();
        let (label, value, widget) = self.field_display(field);
        let f = widgets::Field {
            label,
            value,
            widget,
            focused,
            enabled,
            reason,
            description: field_description(field),
        };
        widgets::field_block(&f, ctrl_col, width)
    }

    fn field_display(&self, field: PField) -> (&'static str, String, &'static str) {
        let a = &self.staged;
        let on_off = |b: bool| {
            if b {
                "on".to_string()
            } else {
                "off".to_string()
            }
        };
        match field {
            PField::Quality => (
                s::P_QUALITY,
                quality_label(&a.quality).to_string(),
                "[select]",
            ),
            PField::Continue => (
                s::P_CONTINUE,
                autoplay_label(&a.autoplay).to_string(),
                "[toggle]",
            ),
            PField::Gapless => (s::P_GAPLESS, on_off(a.gapless), "[toggle]"),
            PField::Restore => (s::P_RESTORE, on_off(a.restore_session), "[toggle]"),
            PField::Resume => (s::P_RESUME_POS, on_off(a.resume_position), "[toggle]"),
            PField::Mpris => (s::P_MPRIS, on_off(a.mpris), "[toggle]"),
        }
    }
}

/// Static one-line help wrapped under a field's label. Only Mpris carries one
/// (it needs a restart to apply, unlike the live-ish other toggles).
fn field_description(field: PField) -> Option<&'static str> {
    match field {
        PField::Mpris => Some(s::P_MPRIS_DESC),
        _ => None,
    }
}

// ============================ mappers ============================

fn quality_label(q: &str) -> &'static str {
    match q {
        "mp3" => s::Q_MP3,
        "cd" => s::Q_CD,
        "hires" => s::Q_HIRES,
        _ => s::Q_HIRES_PLUS,
    }
}

fn autoplay_label(v: &str) -> &'static str {
    match v {
        "track_only" => s::AUTOPLAY_OFF,
        "infinite" => s::AUTOPLAY_INFINITE,
        _ => s::AUTOPLAY_ON,
    }
}

fn autoplay_value(mode: AutoplayMode) -> &'static str {
    match mode {
        AutoplayMode::ContinueWithinSource => "continue",
        AutoplayMode::PlayTrackOnly => "track_only",
        AutoplayMode::InfiniteRadio => "infinite",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> StagedPlayback {
        PlaybackState::new(
            "hires_plus",
            true,
            &AudioSettings::default(),
            &PlaybackPreferences::default(),
        )
        .staged
    }

    #[test]
    fn the_screen_offers_no_key_the_daemon_ignores() {
        // The four rows this screen used to carry in its Quality group wrote
        // keys with no consumer in the daemon, so moving them changed nothing
        // audible. They are gone; this pins that they stay gone, and that every
        // key the screen CAN write is one `settings set` still accepts.
        let mut st = PlaybackState::new(
            "cd",
            false,
            &AudioSettings::default(),
            &PlaybackPreferences::default(),
        );
        st.staged.quality = "mp3".to_string();
        st.staged.autoplay = "track_only".to_string();
        st.staged.gapless = !st.staged.gapless;
        st.staged.restore_session = !st.staged.restore_session;
        st.staged.resume_position = !st.staged.resume_position;
        st.staged.mpris = !st.staged.mpris;

        let written: Vec<String> = st.save_keys().into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            written,
            vec![
                "playback.quality",
                "playback.autoplay",
                "audio.gapless_enabled",
                "playback.persist_session",
                "playback.resume_playback_position",
                "playback.mpris",
            ],
            "every writable key must still be in cli::settings::KEY_TABLE"
        );
    }

    #[test]
    fn gapless_disabled_while_streaming_only_on() {
        let mut p = base();
        p.streaming_only = true;
        let (shown, enabled, reason) = row_state(PField::Gapless, &p);
        assert!(shown && !enabled);
        assert_eq!(reason, Some(s::R_STREAMING_ONLY_ON));
    }

    #[test]
    fn resume_enabled_only_under_restore_session() {
        let mut p = base();
        p.restore_session = false;
        assert!(!row_state(PField::Resume, &p).1);
        p.restore_session = true;
        assert!(row_state(PField::Resume, &p).1);
    }

    #[test]
    fn infinite_autoplay_renders_readonly_label() {
        assert_eq!(autoplay_label("infinite"), s::AUTOPLAY_INFINITE);
    }
}
