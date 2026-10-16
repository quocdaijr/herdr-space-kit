//! The group manager popup: a live tree of groups → spaces → worktrees → agents with edits.
mod app;
mod edit;
mod events;
mod keys;
mod render;

use app::{App, WHEEL_STEP};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind};
use keys::Mode;
use std::io::{Write, stdout};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
const DEBOUNCE: Duration = Duration::from_millis(150);

pub fn run(state_dir: PathBuf) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    // Subscribe before the first snapshot: subscriptions do not replay.
    events::spawn_lifecycle(tx.clone());
    let mut app = App::new(state_dir, tx);
    app.refresh();

    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(stdout(), EnableMouseCapture);
    let mut refetch_at: Option<Instant> = None;
    let result = loop {
        while rx.try_recv().is_ok() {
            refetch_at.get_or_insert(Instant::now() + DEBOUNCE);
        }
        if refetch_at.is_some_and(|t| Instant::now() >= t) {
            refetch_at = None;
            app.refresh();
        }
        if let Err(e) = terminal.draw(|f| render::draw(f, &mut app)) {
            break Err(format!("draw: {e}"));
        }
        if !event::poll(POLL).unwrap_or(false) {
            continue;
        }
        let list_h = terminal.size().map(|s| render::list_height(s.into())).unwrap_or(0);
        let quit = match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => app.handle(keys::map(key, app.mode())),
            Ok(Event::Mouse(m)) if app.mode() == Mode::Normal => match m.kind {
                MouseEventKind::Down(MouseButton::Left) if usize::from(m.row) < list_h => app.click(usize::from(m.row), m.column),
                MouseEventKind::ScrollUp => {
                    app.step(-WHEEL_STEP);
                    false
                }
                MouseEventKind::ScrollDown => {
                    app.step(WHEEL_STEP);
                    false
                }
                _ => false,
            },
            _ => false,
        };
        if quit {
            break Ok(());
        }
    };
    let _ = crossterm::execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Append to `<state_dir>/space-kit.log`; stderr would draw over the TUI.
/// ponytail: unbounded file, rotate if it ever grows.
pub(crate) fn log(msg: &str) {
    let path = crate::model::state_dir().join("space-kit.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{secs} {msg}");
    }
}
