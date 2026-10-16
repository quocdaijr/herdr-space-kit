//! Manager state: snapshot, groups, folds, flattened rows, cursor, prompt, status line.
use super::events::StatusSub;
use super::keys::{Action, Mode};
use crate::herdr::client::{Api, Socket};
use crate::herdr::types::Snapshot;
use crate::model::groups::Groups;
use crate::model::sidebar::Sidebar;
use crate::model::tree::{self, Folds, Kind, Node, Row, Target};
use serde_json::json;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const FLASH: Duration = Duration::from_secs(3);
const DOUBLE_CLICK: Duration = Duration::from_millis(450);
pub const WHEEL_STEP: isize = 3;

pub enum Prompt {
    NewGroup(String),
    Rename { old: String, buf: String },
    Delete(String),
    Move { path_key: String, label: String, options: Vec<String>, idx: usize },
}

pub struct App {
    pub state_dir: PathBuf,
    /// Herdr: the socket, or a fake in tests.
    pub api: Box<dyn Api>,
    pub snap: Snapshot,
    /// `None` until loaded; edits are refused meanwhile so a load error never gets overwritten.
    pub groups: Option<Groups>,
    pub folds: Folds,
    pub nodes: Vec<Node>,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub scroll: usize,
    pub prompt: Option<Prompt>,
    pub help: bool,
    pub tx: Sender<()>,
    status: Option<(String, Instant)>,
    last_click: Option<(usize, Instant)>,
    status_sub: StatusSub,
    placed: bool,
}

impl App {
    pub fn new(state_dir: PathBuf, tx: Sender<()>) -> Self {
        let folds = Folds { sidebar: Sidebar::load(&state_dir), ..Default::default() };
        Self {
            state_dir,
            api: Box::new(Socket),
            snap: Snapshot::default(),
            groups: None,
            folds,
            nodes: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            prompt: None,
            help: false,
            tx,
            status: None,
            last_click: None,
            status_sub: StatusSub::default(),
            placed: false,
        }
    }

    pub fn mode(&self) -> Mode {
        match (&self.prompt, self.help) {
            (_, true) => Mode::Help,
            (None, _) => Mode::Normal,
            (Some(Prompt::NewGroup(_) | Prompt::Rename { .. }), _) => Mode::Input,
            (Some(Prompt::Delete(_)), _) => Mode::Confirm,
            (Some(Prompt::Move { .. }), _) => Mode::Pick,
        }
    }

    /// Error: status line for ~3 s plus the log file.
    pub fn flash(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        super::log(&self.state_dir, &msg);
        self.note(msg);
    }

    /// Informational status line for ~3 s.
    pub fn note(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status.as_ref().filter(|(_, t)| t.elapsed() < FLASH).map(|(m, _)| m.as_str())
    }

    pub fn refresh(&mut self) {
        match crate::native::sync::snapshot(&*self.api) {
            Ok(snap) => self.snap = snap,
            Err(e) => self.flash(format!("snapshot: {e}")),
        }
        // Reload every time: groups may have been edited elsewhere (another popup, a v1 import),
        // and a later edit here must not overwrite that. A failed load keeps what is shown.
        match Groups::load(&self.state_dir, &self.snap) {
            Ok(g) => self.groups = Some(g),
            Err(e) => self.flash(format!("groups.json: {e}")),
        }
        // Folds may change from the sidebar (header clicks, the toggle key) while the popup is open.
        self.folds.sidebar = Sidebar::load(&self.state_dir);
        self.rebuild();
        if !self.placed && !self.rows.is_empty() {
            // Open on the focused Space.
            self.placed = true;
            self.cursor = self.rows.iter().position(|r| r.is_focused && r.kind != Kind::Agent).unwrap_or(0);
        }
        let mut panes: Vec<String> = self.snap.agents.iter().map(|a| a.pane_id.clone()).collect();
        panes.sort();
        self.status_sub.set(panes, &self.tx);
    }

    /// Rebuild rows from in-memory state; the cursor stays on the same row when it still exists.
    pub fn rebuild(&mut self) {
        let anchor = self.rows.get(self.cursor).map(|r| (r.key.clone(), r.target.clone()));
        let empty = Groups::default();
        self.nodes = tree::build(&self.snap, self.groups.as_ref().unwrap_or(&empty), &self.folds);
        self.rows = tree::visible(&self.nodes);
        let same = anchor.and_then(|a| self.rows.iter().position(|r| (r.key.clone(), r.target.clone()) == a));
        self.cursor = same.unwrap_or(self.cursor).min(self.rows.len().saturating_sub(1));
    }

    /// Returns true when the popup should close.
    pub fn handle(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => return true,
            Action::Up => self.step(-1),
            Action::Down => self.step(1),
            Action::Jump => return self.jump(),
            Action::Toggle => self.collapse(None),
            Action::Collapse => self.collapse(Some(true)),
            Action::Expand => self.collapse(Some(false)),
            Action::CollapseAll => self.collapse_all(true),
            Action::ExpandAll => self.collapse_all(false),
            Action::Refresh => self.refresh(),
            Action::Help => self.help = !self.help,
            Action::Cancel => self.prompt = None,
            Action::None => {}
            other => self.edit(other),
        }
        false
    }

    /// Cursor step, or picker step while the move picker is open.
    pub fn step(&mut self, delta: isize) {
        if let Some(Prompt::Move { options, idx, .. }) = &mut self.prompt {
            *idx = idx.saturating_add_signed(delta).min(options.len().saturating_sub(1));
            return;
        }
        self.cursor = self.cursor.saturating_add_signed(delta).min(self.rows.len().saturating_sub(1));
    }

    /// Focus the row's workspace or agent; true (close the popup) when that worked.
    fn jump(&mut self) -> bool {
        let Some(target) = self.rows.get(self.cursor).and_then(|r| r.target.clone()) else { return false };
        let result = match &target {
            Target::Workspace(id) => self.api.call("workspace.focus", json!({"workspace_id": id})),
            Target::Pane(id) => self
                .api
                .call("agent.focus", json!({"target": id}))
                .or_else(|_| self.api.call("pane.focus", json!({"pane_id": id}))),
        };
        match result {
            Ok(_) => true,
            Err(e) => {
                self.flash(format!("jump: {e}"));
                false
            }
        }
    }

    /// `want`: Some(true) fold, Some(false) unfold, None toggle. Group folds are the sidebar's.
    fn collapse(&mut self, want: Option<bool>) {
        let Some(key) = self.rows.get(self.cursor).filter(|r| r.collapsible).and_then(|r| r.key.clone()) else {
            return;
        };
        let is = self.folds.is_collapsed(&key);
        if want.unwrap_or(!is) == is {
            return;
        }
        match key.strip_prefix("g:") {
            Some(group) => {
                let Some(groups) = &self.groups else { return };
                self.set_sidebar(self.folds.sidebar.toggle(group, groups));
            }
            None => {
                if !self.folds.spaces.remove(&key) {
                    self.folds.spaces.insert(key);
                }
                self.rebuild();
            }
        }
    }

    /// H folds every group (an outline); L unfolds everything.
    fn collapse_all(&mut self, fold: bool) {
        let collapsed = match fold {
            true => self.groups.iter().flat_map(|g| &g.groups).map(|g| g.name.clone()).collect(),
            false => Vec::new(),
        };
        if !fold {
            self.folds.spaces.clear();
        }
        self.set_sidebar(Sidebar { collapsed, ..self.folds.sidebar.clone() });
    }

    /// Save sidebar folds, then sync so Herdr's Spaces list folds too.
    pub fn set_sidebar(&mut self, sidebar: Sidebar) {
        if let Err(e) = sidebar.save(&self.state_dir) {
            return self.flash(format!("sidebar.json: {e}"));
        }
        self.folds.sidebar = sidebar;
        self.rebuild();
        self.sync_native();
    }

    /// Left click on visible row `row` at column `col`: the fold arrow folds, a double click jumps.
    /// Returns true when the popup should close.
    pub fn click(&mut self, row: usize, col: u16) -> bool {
        let idx = self.scroll + row;
        let Some(r) = self.rows.get(idx) else { return false };
        let lead = r.text.chars().take_while(|c| *c == ' ').count();
        let on_arrow = r.collapsible && usize::from(col) <= lead.max(1);
        let double = self.last_click.is_some_and(|(i, t)| i == idx && t.elapsed() < DOUBLE_CLICK);
        self.cursor = idx;
        if on_arrow {
            self.last_click = None;
            self.collapse(None);
        } else if double {
            self.last_click = None;
            return self.jump();
        } else {
            self.last_click = Some((idx, Instant::now()));
        }
        false
    }

    /// Rewrite Herdr's sidebar from the just-saved state. Fast (~10 ms), so it runs inline.
    pub fn sync_native(&mut self) {
        if let Err(e) = crate::native::sync::sync(&*self.api, &self.state_dir, false, true) {
            self.flash(format!("sync: {e}"));
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::herdr::types::{Agent, AgentStatus, Workspace, Worktree};
    use crate::model::groups::PathKey;

    fn ws(id: &str, label: &str, root: &str, linked: bool) -> Workspace {
        let worktree = Worktree {
            repo_root: root.into(),
            repo_key: format!("{root}/.git"),
            repo_name: "repo".into(),
            checkout_path: format!("{root}-{id}"),
            is_linked_worktree: linked,
        };
        Workspace { workspace_id: id.into(), label: label.into(), worktree: Some(worktree) }
    }

    /// An app on a temp state dir: group Work holds `api` (one agent, one worktree); `notes` is
    /// ungrouped. Rows: Work, api, its agent, its worktree, Ungrouped, notes.
    pub fn app(name: &str) -> App {
        let dir = crate::native::report::tests::temp_dir(name);
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(dir, tx);
        let api = ws("a", "api", "/space-kit-app/api", false);
        let key = PathKey::for_workspace(&api, &[]);
        app.snap = Snapshot {
            focused_workspace_id: Some("a".into()),
            workspaces: vec![
                ws("h", "⟦Work⟧", "/space-kit-app/h", false),
                api,
                ws("wt", "feat", "/space-kit-app/api", true),
                ws("n", "notes", "/space-kit-app/n", false),
            ],
            agents: vec![Agent {
                pane_id: "a:p1".into(),
                workspace_id: "a".into(),
                agent: "claude".into(),
                agent_status: AgentStatus::Working,
                terminal_title_stripped: None,
            }],
            ..Default::default()
        };
        let groups = Groups::default().create("Work").unwrap().move_to(key.as_str(), Some("Work"));
        groups.save(&app.state_dir).unwrap();
        app.groups = Some(groups);
        app.rebuild();
        app
    }

    fn texts(app: &App) -> Vec<String> {
        app.rows.iter().map(|r| r.text.trim().to_owned()).collect()
    }

    #[test]
    fn rows_cursor_and_modes() {
        let mut app = app("app-modes");
        assert_eq!(texts(&app), ["▾ Work ·1 ●1", "● api", "● claude", "└─ · feat", "▾ Ungrouped ·1", "notes"]);
        assert_eq!(app.mode(), Mode::Normal);
        app.handle(Action::Up);
        assert_eq!(app.cursor, 0, "the cursor stops at the top");
        (0..10).for_each(|_| app.step(1));
        assert_eq!(app.cursor, 5, "and at the bottom");
        assert!(!app.handle(Action::Help));
        assert_eq!(app.mode(), Mode::Help);
        app.handle(Action::Help);
        app.prompt = Some(Prompt::Delete("Work".into()));
        assert_eq!(app.mode(), Mode::Confirm);
        app.handle(Action::Cancel);
        assert!(app.prompt.is_none());
        assert!(app.handle(Action::Quit));
    }

    #[test]
    fn folding_a_group_saves_it_for_the_sidebar() {
        let mut app = app("app-group-fold");
        app.cursor = 0;
        app.handle(Action::Toggle);
        assert_eq!(Sidebar::load(&app.state_dir).collapsed, ["Work"]);
        assert_eq!(texts(&app), ["▸ Work ·1 ●1", "▾ Ungrouped ·1", "notes"]);
        app.handle(Action::Collapse);
        assert_eq!(Sidebar::load(&app.state_dir).collapsed, ["Work"], "folding a folded group is a no-op");
        app.handle(Action::Expand);
        assert!(Sidebar::load(&app.state_dir).collapsed.is_empty());
        app.cursor = 4;
        app.handle(Action::Toggle);
        assert!(Sidebar::load(&app.state_dir).collapsed.is_empty(), "Ungrouped cannot fold");
    }

    #[test]
    fn space_folds_stay_in_the_popup_and_fold_all_unfolds_them() {
        let mut app = app("app-space-fold");
        app.cursor = 1;
        app.handle(Action::Collapse);
        assert_eq!(texts(&app), ["▾ Work ·1 ●1", "● api", "▾ Ungrouped ·1", "notes"]);
        assert!(Sidebar::load(&app.state_dir).collapsed.is_empty(), "space folds are not saved");
        app.handle(Action::CollapseAll);
        assert_eq!(Sidebar::load(&app.state_dir).collapsed, ["Work"]);
        app.handle(Action::ExpandAll);
        assert!(app.folds.spaces.is_empty() && Sidebar::load(&app.state_dir).collapsed.is_empty());
        assert_eq!(app.rows.len(), 6);
    }

    #[test]
    fn clicks_select_rows_and_the_arrow_folds() {
        let mut app = app("app-click");
        assert!(!app.click(5, 10));
        assert_eq!(app.cursor, 5, "a click selects the row");
        assert!(!app.click(9, 0), "below the list: nothing");
        assert!(!app.click(0, 0), "the group arrow folds");
        assert_eq!(Sidebar::load(&app.state_dir).collapsed, ["Work"]);
    }

    #[test]
    fn the_move_picker_steps_within_its_options() {
        let mut app = app("app-picker");
        app.prompt = Some(Prompt::Move {
            path_key: "/x".into(),
            label: "notes".into(),
            options: vec!["Work".into(), "Ungrouped".into()],
            idx: 0,
        });
        assert_eq!(app.mode(), Mode::Pick);
        app.step(5);
        app.step(-1);
        let Some(Prompt::Move { idx, .. }) = &app.prompt else { panic!() };
        assert_eq!(*idx, 0);
        app.flash("boom");
        assert_eq!(app.status_text(), Some("boom"));
    }

    /// A Herdr that answers every call with an error.
    struct Down;

    impl Api for Down {
        fn call(&self, method: &str, _: serde_json::Value) -> crate::herdr::client::Result<serde_json::Value> {
            Err(format!("{method}: herdr is down"))
        }
    }

    fn row_at(app: &App, text: &str) -> usize {
        app.rows.iter().position(|r| r.text.trim() == text).unwrap()
    }

    #[test]
    fn refresh_reads_the_snapshot_groups_and_folds() {
        let mut app = app("app-refresh");
        Sidebar { collapsed: vec!["Work".into()], ..Default::default() }.save(&app.state_dir).unwrap();
        app.refresh();
        assert_eq!(app.folds.sidebar.collapsed, ["Work"], "folds made elsewhere are picked up");
        assert_eq!(app.snap.workspaces.len(), 2, "the fake Herdr's snapshot");

        std::fs::write(app.state_dir.join("groups.json"), "{broken").unwrap();
        app.api = Box::new(Down);
        app.refresh();
        assert!(app.groups.is_some(), "a failed load keeps what is shown");
        assert!(app.status_text().unwrap().starts_with("groups.json:"));
        let log = std::fs::read_to_string(app.state_dir.join("space-kit.log")).unwrap();
        assert!(log.contains("snapshot: session.snapshot: herdr is down"), "{log}");
    }

    #[test]
    fn jump_focuses_the_row_and_closes_or_reports_the_failure() {
        let mut app = app("app-jump");
        app.cursor = row_at(&app, "● api");
        assert!(app.handle(Action::Jump), "workspace focus worked: close");
        app.cursor = 0;
        assert!(!app.handle(Action::Jump), "a group row has no target");

        let mut agent = app.rows[row_at(&app, "● claude")].clone();
        agent.target = Some(Target::Pane("fail".into()));
        app.rows.push(agent);
        app.cursor = app.rows.len() - 1;
        assert!(app.handle(Action::Jump), "agent.focus failed, pane.focus worked");

        app.api = Box::new(Down);
        assert!(!app.handle(Action::Jump));
        assert_eq!(app.status_text(), Some("jump: pane.focus: herdr is down"));
    }

    #[test]
    fn a_double_click_jumps() {
        let mut app = app("app-double");
        let api = row_at(&app, "● api");
        assert!(!app.click(api, 10));
        assert!(app.click(api, 10), "second click on the same row jumps (and closes)");
    }

    #[test]
    fn fold_save_and_sync_failures_are_reported() {
        let mut app = app("app-fail");
        app.api = Box::new(Down);
        app.cursor = 0;
        app.handle(Action::Toggle);
        assert!(app.status_text().unwrap().starts_with("sync: session.snapshot: herdr is down"));
        app.state_dir = app.state_dir.join("groups.json"); // a file: nothing can be saved under it
        app.handle(Action::Toggle);
        assert!(app.status_text().unwrap().starts_with("sidebar.json:"));
    }

    #[test]
    fn refresh_and_unmapped_keys() {
        let mut app = app("app-keys");
        assert!(!app.handle(Action::None));
        assert!(!app.handle(Action::Refresh));
        assert_eq!(app.snap.workspaces.len(), 2, "refreshed from the fake Herdr");
    }
}
