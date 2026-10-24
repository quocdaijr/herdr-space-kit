//! The group overview that lives in a header workspace's pane. Clicking a header keeps focus on
//! it (so Herdr's sidebar does not scroll away), and this pane shows what is in the group: open a
//! project with Enter or a click, fold or unfold the group with Space or a click on the title.
use crate::herdr::client::{Api, Socket};
use crate::herdr::types::{AgentStatus, Snapshot};
use crate::model::groups::Groups;
use crate::model::sidebar::Sidebar;
use crate::native::headers::{self, Split};
use crate::native::order::sections;
use crate::native::sync;
use crate::text::{fit_row, state_counts};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
/// ponytail: agent states refresh on a timer, not per-pane status subscriptions like the manager.
const TICK: Duration = Duration::from_secs(2);
/// Rows above the list: the title (click to fold/unfold) and the key hint.
const TOP: usize = 2;
const HINT: &str = "↑↓ move · Enter/click open · Space fold/unfold";

#[derive(Debug, PartialEq)]
pub struct Row {
    pub workspace_id: String,
    pub prefix: String,
    pub label: String,
    pub counts: String,
}

#[derive(Debug, PartialEq)]
pub struct Overview {
    pub group: String,
    pub folded: bool,
    /// Top-level entries, as the sidebar header counts them (worktrees are rows, not projects).
    pub projects: usize,
    pub rows: Vec<Row>,
}

/// The overview of the group `header_id` leads; None when it is not (or no longer) a header.
pub fn build(snap: &Snapshot, groups: &Groups, sidebar: &Sidebar, header_id: &str) -> Option<Overview> {
    let header = snap.workspaces.iter().find(|w| w.workspace_id == header_id)?;
    let group = headers::group_of(header)?;
    let split = Split::of(&snap.workspaces);
    let all = sections(&split.projects, &snap.panes, groups);
    let section = all.into_iter().find(|s| s.group == Some(group));
    let states = |id: &str| -> Vec<AgentStatus> {
        snap.agents.iter().filter(|a| a.workspace_id == id).map(|a| a.agent_status).collect()
    };
    let mut rows = Vec::new();
    let entries = section.map(|s| s.entries).unwrap_or_default();
    for (i, entry) in entries.iter().enumerate() {
        let last = i + 1 == entries.len();
        let (tree, stem) = if last { ("└─ ", "     ") } else { ("├─ ", "│    ") };
        for (j, ws) in std::iter::once(entry.ws).chain(entry.worktrees.iter().copied()).enumerate() {
            rows.push(Row {
                workspace_id: ws.workspace_id.clone(),
                prefix: if j == 0 { tree } else { stem }.to_owned(),
                label: ws.label.clone(),
                counts: state_counts(&states(&ws.workspace_id)),
            });
        }
    }
    Some(Overview { group: group.to_owned(), folded: sidebar.is_folded(group), projects: entries.len(), rows })
}

pub struct App<'a> {
    api: &'a dyn Api,
    dir: PathBuf,
    header_id: String,
    pub view: Option<Overview>,
    pub cursor: usize,
    pub status: String,
}

impl<'a> App<'a> {
    pub fn new(api: &'a dyn Api, dir: PathBuf, header_id: String) -> Self {
        Self { api, dir, header_id, view: None, cursor: 0, status: String::new() }
    }

    pub fn refresh(&mut self) {
        let built = sync::snapshot(self.api).and_then(|snap| {
            let groups = Groups::load(&self.dir, &snap).map_err(|e| format!("groups.json: {e}"))?;
            Ok(build(&snap, &groups, &Sidebar::load(&self.dir), &self.header_id))
        });
        match built {
            Ok(view) => {
                let n = view.as_ref().map_or(0, |v| v.rows.len());
                self.cursor = self.cursor.min(n.saturating_sub(1));
                self.view = view;
            }
            Err(e) => self.status = e,
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.view.as_ref().map_or(0, |v| v.rows.len());
        self.cursor = self.cursor.saturating_add_signed(delta).min(n.saturating_sub(1));
    }

    fn open(&mut self) {
        let Some(id) = self.view.as_ref().and_then(|v| v.rows.get(self.cursor)).map(|r| r.workspace_id.clone()) else {
            return;
        };
        if let Err(e) = self.api.call("workspace.focus", serde_json::json!({"workspace_id": id})) {
            self.status = e;
        }
    }

    fn toggle(&mut self) {
        let Some(group) = self.view.as_ref().map(|v| v.group.clone()) else { return };
        if let Err(e) = sync::toggle_group(self.api, &self.dir, &group) {
            self.status = e;
        }
        self.refresh();
    }

    pub fn key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Enter => self.open(),
            KeyCode::Char(' ') => self.toggle(),
            _ => {}
        }
    }

    /// A left click on screen row `row`: the title toggles, a list row opens.
    pub fn click(&mut self, row: usize) {
        if row == 0 {
            self.toggle();
        } else if let Some(i) = row.checked_sub(TOP)
            && self.view.as_ref().is_some_and(|v| i < v.rows.len())
        {
            self.cursor = i;
            self.open();
        }
    }

    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(view) = &self.view else {
            return vec![Line::raw("Not a group header any more."), Line::raw(self.status.clone())];
        };
        let state = if view.folded { "folded" } else { "unfolded" };
        let action = if view.folded { "[ Space: unfold ]" } else { "[ Space: fold ]" };
        let title = format!("━━ {} · {} · {state}", view.group, plural(view.projects));
        let gap = width.saturating_sub(crate::text::width(&title) + crate::text::width(action)).max(1);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut lines = vec![
            Line::styled(format!("{title}{}{action}", " ".repeat(gap)), bold),
            Line::styled(
                if self.status.is_empty() { HINT.to_owned() } else { self.status.clone() },
                Style::default().add_modifier(Modifier::DIM),
            ),
        ];
        for (i, row) in view.rows.iter().enumerate() {
            let text = fit_row(&row.prefix, &row.label, &row.counts, width);
            let style =
                if i == self.cursor { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
            lines.push(Line::styled(text, style));
        }
        lines
    }

    /// Returns nothing; the overview never quits on its own (it lives as long as its header).
    pub fn input(&mut self, input: Event) {
        match input {
            Event::Key(k) if k.kind == KeyEventKind::Press => self.key(k.code),
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => self.click(usize::from(m.row)),
                MouseEventKind::ScrollUp => self.step(-1),
                MouseEventKind::ScrollDown => self.step(1),
                _ => {}
            },
            _ => {}
        }
    }
}

fn plural(n: usize) -> String {
    if n == 1 { "1 project".into() } else { format!("{n} projects") }
}

pub fn run(dir: PathBuf, header_id: String) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    crate::tui::events::spawn_lifecycle(tx);
    let api = Socket;
    let mut app = App::new(&api, dir, header_id);
    app.refresh();
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(stdout(), EnableMouseCapture);
    let next_input = || if event::poll(POLL).unwrap_or(false) { event::read().ok() } else { None };
    let result = event_loop(&mut app, &mut terminal, &rx, next_input, || false);
    let _ = crossterm::execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Draw and handle input until `stop` says so (never, outside tests). Herdr events and the tick
/// refetch the snapshot.
fn event_loop<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    rx: &Receiver<()>,
    mut next_input: impl FnMut() -> Option<Event>,
    stop: impl Fn() -> bool,
) -> Result<(), String> {
    let mut refetch_at = Instant::now() + TICK;
    while !stop() {
        if rx.try_iter().count() > 0 || Instant::now() >= refetch_at {
            refetch_at = Instant::now() + TICK;
            app.refresh();
        }
        terminal
            .draw(|f| {
                let area = f.area();
                let text = ratatui::text::Text::from(app.lines(usize::from(area.width)));
                f.render_widget(ratatui::widgets::Paragraph::new(text), area);
            })
            .map_err(|e| format!("draw: {e}"))?;
        if let Some(input) = next_input() {
            app.input(input);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::groups::PathKey;
    use crate::native::report::tests::{Fake, temp_dir};
    use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent};
    use ratatui::backend::TestBackend;
    use serde_json::{Value, json};
    use std::cell::Cell;

    fn snap() -> Value {
        json!({
            "focused_workspace_id": "h1",
            "workspaces": [
                {"workspace_id": "h1", "label": "⟦A⟧"},
                {"workspace_id": "p1", "label": "web", "worktree": {"repo_root": "/tmp/ov-web", "repo_key": "/tmp/ov-web/.git"}},
                {"workspace_id": "wt", "label": "fix", "worktree": {"repo_root": "/tmp/ov-web", "repo_key": "/tmp/ov-web/.git", "is_linked_worktree": true}},
                {"workspace_id": "p2", "label": "api", "worktree": {"repo_root": "/tmp/ov-api", "repo_key": "/tmp/ov-api/.git"}},
            ],
            "panes": [],
            "agents": [{"pane_id": "wt:1", "workspace_id": "wt", "agent": "claude", "agent_status": "blocked"}],
        })
    }

    fn setup(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        let s: Snapshot = serde_json::from_value(snap()).unwrap();
        let key = |i: usize| PathKey::for_workspace(&s.workspaces[i], &[]).as_str().to_owned();
        let groups = Groups::default().create("A").unwrap().move_to(&key(1), Some("A")).move_to(&key(3), Some("A"));
        groups.save(&dir).unwrap();
        dir
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn rows_list_the_group_with_worktrees_and_agent_states() {
        let dir = setup("ov-build");
        let s: Snapshot = serde_json::from_value(snap()).unwrap();
        let groups = Groups::load(&dir, &s).unwrap();
        let view = build(&s, &groups, &Sidebar::default(), "h1").unwrap();
        assert_eq!(view.group, "A");
        assert!(!view.folded);
        let rows: Vec<(&str, &str)> = view.rows.iter().map(|r| (r.prefix.as_str(), r.label.as_str())).collect();
        assert_eq!(rows, [("├─ ", "web"), ("│    ", "fix"), ("└─ ", "api")]);
        assert!(view.rows[1].counts.contains('1'), "the blocked agent is counted: {:?}", view.rows[1].counts);
        assert_eq!(build(&s, &groups, &Sidebar::default(), "p1"), None, "a project is no header");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys_and_clicks_move_open_and_fold() {
        let dir = setup("ov-keys");
        let api = Fake { snapshot: snap(), ..Default::default() };
        let mut app = App::new(&api, dir.clone(), "h1".into());
        app.refresh();
        app.input(key(KeyCode::Down));
        app.input(key(KeyCode::Char('j')));
        app.input(key(KeyCode::Char('j')));
        assert_eq!(app.cursor, 2, "stops at the last row");
        app.input(key(KeyCode::Up));
        app.input(key(KeyCode::Enter));
        assert!(api.sent.borrow().iter().any(|(m, p)| m == "workspace.focus" && p["workspace_id"] == "wt"));

        app.input(key(KeyCode::Char(' ')));
        assert_eq!(Sidebar::load(&dir).collapsed, ["A"]);
        assert!(app.view.as_ref().unwrap().folded, "refreshed after the fold");
        app.input(click(0));
        assert!(Sidebar::load(&dir).collapsed.is_empty(), "a click on the title unfolds");

        app.input(click(4));
        assert_eq!(app.cursor, 2);
        assert!(api.sent.borrow().iter().any(|(m, p)| m == "workspace.focus" && p["workspace_id"] == "p2"));
        app.input(click(1)); // the hint line does nothing
        app.input(click(9)); // below the list
        assert_eq!(app.cursor, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn errors_show_in_the_hint_line_and_a_gone_header_says_so() {
        let dir = setup("ov-errors");
        let api = Fake { snapshot: snap(), fail: &["workspace.focus"], ..Default::default() };
        let mut app = App::new(&api, dir.clone(), "h1".into());
        app.refresh();
        app.input(key(KeyCode::Enter));
        assert!(app.status.contains("refused"));
        assert!(app.lines(60)[1].to_string().contains("refused"));

        let gone = Fake { snapshot: snap(), fail: &["session.snapshot"], ..Default::default() };
        let mut app = App::new(&gone, dir.clone(), "h9".into());
        app.refresh();
        app.input(key(KeyCode::Enter));
        app.input(key(KeyCode::Char(' ')));
        assert!(app.lines(60)[0].to_string().starts_with("Not a group header"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_loop_draws_the_title_and_rows() {
        let dir = setup("ov-loop");
        let api = Fake { snapshot: snap(), ..Default::default() };
        let mut app = App::new(&api, dir.clone(), "h1".into());
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(()).unwrap();
        let mut script = vec![
            Some(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })),
            Some(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })),
            Some(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 0,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })),
            Some(Event::FocusGained),
            None,
        ]
        .into_iter();
        let turns = Cell::new(0);
        let stop = || {
            turns.set(turns.get() + 1);
            turns.get() > 6
        };
        event_loop(&mut app, &mut terminal, &rx, || script.next().flatten(), stop).unwrap();
        let row = |y: u16| (0..60).map(|x| terminal.backend().buffer()[(x, y)].symbol().to_owned()).collect::<String>();
        assert!(row(0).starts_with("━━ A · 2 projects · unfolded"), "{}", row(0));
        assert!(row(0).trim_end().ends_with("[ Space: fold ]"));
        assert!(row(2).starts_with("├─ web"));
        assert_eq!(plural(1), "1 project");
        let _ = std::fs::remove_dir_all(dir);
    }
}
