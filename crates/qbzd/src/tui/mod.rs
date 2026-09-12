// crates/qbzd/src/tui/mod.rs — `qbzd setup` entry, terminal lifecycle, event loop
// (03-setup-tui.md §2). Non-tty is rejected before any terminal mutation (§2.4).
// `ratatui::init()` enables raw mode + the alternate screen AND installs a panic
// hook that restores the terminal on any panic (§2.1 — a wedged terminal over
// SSH is a support fire); `ratatui::restore()` restores on the normal exit path.
//
// The event loop runs synchronously on a dedicated blocking thread (spawned off
// the tokio runtime), holding a runtime `Handle` so the discrete I/O actions
// (§5.5: screen entry, `r`, save, immediate actions) run on workers or a
// short-lived `block_on` — never on a keystroke.

pub mod app;
pub mod clipboard;
pub mod strings;
pub mod theme;
pub mod widgets;
pub mod wizard_core;

mod screens;

use std::io::IsTerminal;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use tokio::runtime::Handle;

use crate::paths::ProfileRoots;
use app::{App, LoopCmd};

/// Poll cadence — also the spinner tick while a worker runs (§5.5). No I/O here;
/// the loop only redraws and drains the worker channel.
const TICK: Duration = Duration::from_millis(120);

/// `qbzd setup` entry. Exit 2 on a non-tty (§2.4); else runs the configurator and
/// returns 0.
pub async fn run(roots: ProfileRoots) -> i32 {
    // §2.4: reject a non-interactive invocation BEFORE touching the terminal.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("{}", strings::NON_TTY_ERROR);
        return 2;
    }
    let handle = Handle::current();
    // The synchronous ratatui loop runs on a blocking thread so its `block_on`
    // calls are legal (a runtime worker thread cannot block_on itself).
    tokio::task::spawn_blocking(move || run_sync(roots, handle))
        .await
        .unwrap_or(1)
}

fn run_sync(roots: ProfileRoots, handle: Handle) -> i32 {
    let mut terminal = ratatui::init();
    let mut app = App::new(roots, handle.clone());
    let code = event_loop(&mut terminal, &mut app, &handle);
    ratatui::restore();
    code
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, _handle: &Handle) -> i32 {
    loop {
        if terminal.draw(|f| app.draw(f)).is_err() {
            return 1;
        }
        app.drain_worker();
        if app.busy() {
            app.busy_tick = app.busy_tick.wrapping_add(1);
        }

        if event::poll(TICK).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match app.on_key(key) {
                    LoopCmd::None => {}
                },
                Ok(_) => {} // resize/mouse/etc. — the next draw re-lays-out (§5.4)
                Err(_) => return 1,
            }
        }

        if app.should_quit() {
            return 0;
        }
    }
}
