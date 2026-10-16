//! Manager state: snapshot, groups, folds, flattened rows, cursor, prompt, status line.
use super::events::StatusSub;
use super::keys::{Action, Mode};
use crate::herdr::client;
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
        super::log(&msg);
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
        match crate::native::sync::snapshot(&client::Socket) {
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
            Target::Workspace(id) => client::request("workspace.focus", json!({"workspace_id": id})),
            Target::Pane(id) => client::request("agent.focus", json!({"target": id}))
                .or_else(|_| client::request("pane.focus", json!({"pane_id": id}))),
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
        if cfg!(test) {
            return; // unit tests must never write to the live Herdr server
        }
        if let Err(e) = crate::native::sync::sync(&client::Socket, &self.state_dir, false, true) {
            self.flash(format!("sync: {e}"));
        }
    }
}
