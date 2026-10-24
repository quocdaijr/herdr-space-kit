//! The group manager popup: a live tree of groups → spaces → worktrees → agents with edits.
mod app;
mod edit;
pub(crate) mod events;
mod keys;
mod render;

use app::{App, WHEEL_STEP};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
};
use keys::Mode;
use ratatui::Terminal;
use ratatui::backend::Backend;
use std::io::{Write, stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
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
    release_mouse_on_panic();
    let next_input = || if event::poll(POLL).unwrap_or(false) { event::read().ok() } else { None };
    let result = event_loop(&mut app, &mut terminal, &rx, next_input);
    let _ = crossterm::execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// ratatui's panic hook restores the terminal but not mouse capture; turn it off first.
fn release_mouse_on_panic() {
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(stdout(), DisableMouseCapture);
        restore(info);
    }));
}

/// Draw, then handle one input (`None`: nothing arrived this tick) until the popup closes.
/// Herdr events arriving on `rx` refetch the snapshot, debounced.
fn event_loop<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    rx: &Receiver<()>,
    mut next_input: impl FnMut() -> Option<Event>,
) -> Result<(), String> {
    let mut refetch_at: Option<Instant> = None;
    loop {
        while rx.try_recv().is_ok() {
            refetch_at.get_or_insert(Instant::now() + DEBOUNCE);
        }
        if refetch_at.is_some_and(|t| Instant::now() >= t) {
            refetch_at = None;
            app.refresh();
        }
        terminal.draw(|f| render::draw(f, app)).map_err(|e| format!("draw: {e}"))?;
        let Some(input) = next_input() else { continue };
        let list_h = terminal.size().map(|s| render::list_height(s.into())).unwrap_or(0);
        if on_input(app, input, list_h) {
            return Ok(());
        }
    }
}

/// Returns true when the popup should close.
fn on_input(app: &mut App, input: Event, list_h: usize) -> bool {
    match input {
        Event::Key(key) if key.kind == KeyEventKind::Press => app.handle(keys::map(key, app.mode())),
        Event::Mouse(m) if app.mode() == Mode::Normal => match m.kind {
            MouseEventKind::Down(MouseButton::Left) if usize::from(m.row) < list_h => {
                app.click(usize::from(m.row), m.column)
            }
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
    }
}

/// Append to `<state_dir>/space-kit.log`; stderr would draw over the TUI.
/// ponytail: unbounded file, rotate if it ever grows.
pub(crate) fn log(state_dir: &std::path::Path, msg: &str) {
    let path = state_dir.join("space-kit.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{secs} {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
    use ratatui::backend::TestBackend;

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn mouse(kind: MouseEventKind, row: u16) -> Event {
        Event::Mouse(MouseEvent { kind, column: 10, row, modifiers: KeyModifiers::NONE })
    }

    #[test]
    fn the_loop_draws_handles_input_refetches_and_quits() {
        let mut app = app::tests::app("loop");
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(()).unwrap(); // a Herdr event: refetch after the debounce
        let mut script = vec![
            Some(key('j')),
            Some(mouse(MouseEventKind::ScrollDown, 0)),
            Some(mouse(MouseEventKind::ScrollUp, 0)),
            Some(mouse(MouseEventKind::Down(MouseButton::Left), 5)),
            Some(mouse(MouseEventKind::Down(MouseButton::Left), 30)), // below the list
            Some(mouse(MouseEventKind::Moved, 0)),
            Some(Event::FocusGained),
            None,
            Some(key('q')),
        ]
        .into_iter();
        let next = || {
            let input = script.next().flatten();
            if input.is_none() {
                std::thread::sleep(DEBOUNCE * 2); // let the refetch fire
            }
            input
        };
        assert_eq!(event_loop(&mut app, &mut terminal, &rx, next), Ok(()));
        assert_eq!(app.cursor, 1, "refetched from the fake Herdr, cursor kept in range");
        let screen: String = (0..40).map(|x| terminal.backend().buffer()[(x, 0)].symbol().to_owned()).collect();
        assert!(screen.starts_with("▾ Ungrouped"), "the refetch replaced the fixture: {screen}");
    }

    #[test]
    fn mouse_input_is_ignored_while_a_prompt_is_open() {
        let mut app = app::tests::app("loop-prompt");
        app.prompt = Some(app::Prompt::NewGroup(String::new()));
        assert!(!on_input(&mut app, mouse(MouseEventKind::ScrollDown, 0), 9));
        assert_eq!(app.cursor, 0);
        assert!(!on_input(&mut app, key('q'), 9), "q types into the prompt");
        assert!(matches!(&app.prompt, Some(app::Prompt::NewGroup(buf)) if buf == "q"));
    }

    #[test]
    fn log_appends_to_the_state_dir() {
        let dir = crate::native::report::tests::temp_dir("log");
        log(&dir, "hello");
        assert!(std::fs::read_to_string(dir.join("space-kit.log")).unwrap().ends_with(" hello\n"));
    }

    #[test]
    fn a_panic_releases_the_mouse_then_runs_the_previous_hook() {
        release_mouse_on_panic();
        assert!(std::panic::catch_unwind(|| panic!("boom")).is_err());
        let _ = std::panic::take_hook(); // back to the default hook
    }
}
