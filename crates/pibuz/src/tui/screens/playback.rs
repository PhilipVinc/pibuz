// crates/pibuz/src/tui/screens/playback.rs — the Playback screen (03 §3.3).
//
// Reads two stores at entry (daemon_prefs and AudioSettings) and writes back
// through the App's write_one path.
//
// The Quality group used to carry four more rows — a device-rate limiter, its
// max-rate select, an allow-fallback toggle and a retry-on-failure select, plus
// the `ask` rendering rule that hung off the last of them. They are gone with
// the keys behind them (see `cli::settings::KEY_TABLE`): the daemon reads none
// of the four, so every one of them was a control the operator could move
// without anything changing.
//
// The Continue, Restore-session and Resume-position rows went the same way:
// they wrote the desktop's playback preferences, which the daemon never read.

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
    pub quality: String, // playback.quality
    pub gapless: bool,   // audio.gapless_enabled
    pub mpris: bool,     // playback.mpris (applies on restart)
    /// Read-only, from the audio store — drives the Gapless disabled reason.
    pub streaming_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PField {
    Quality,
    Gapless,
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
        _ => (true, true, None),
    }
}

pub fn visible_fields(p: &StagedPlayback) -> Vec<PField> {
    use PField::*;
    [Quality, Gapless, Mpris]
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
    pub fn new(quality: &str, mpris: bool, audio: &AudioSettings) -> Self {
        let staged = StagedPlayback {
            quality: quality.to_string(),
            gapless: audio.gapless_enabled,
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
        if a.gapless != b.gapless {
            out.push(("audio.gapless_enabled".to_string(), a.gapless.to_string()));
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
            PField::Gapless => self.staged.gapless ^= true,
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

        let behavior: &[PField] = &[Gapless];
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
            PField::Gapless => (s::P_GAPLESS, on_off(a.gapless), "[toggle]"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> StagedPlayback {
        PlaybackState::new("hires_plus", true, &AudioSettings::default()).staged
    }

    #[test]
    fn the_screen_offers_no_key_the_daemon_ignores() {
        // The rows this screen used to carry in its Quality group wrote
        // keys with no consumer in the daemon, so moving them changed nothing
        // audible. They are gone; this pins that they stay gone, and that every
        // key the screen CAN write is one `settings set` still accepts.
        let mut st = PlaybackState::new("cd", false, &AudioSettings::default());
        st.staged.quality = "mp3".to_string();
        st.staged.gapless = !st.staged.gapless;
        st.staged.mpris = !st.staged.mpris;

        let written: Vec<String> = st.save_keys().into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            written,
            vec![
                "playback.quality",
                "audio.gapless_enabled",
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
}
