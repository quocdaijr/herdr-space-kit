//! Group → Space → Worktree → Agent tree for the manager popup, built from a snapshot.
//! `build` returns the complete model; `visible` flattens it, dropping folded descendants.
//! Row text is untruncated; the TUI fits it to the pane width.
use super::groups::{Groups, PathKey, UNGROUPED};
use super::realpath;
use super::sidebar::Sidebar;
use crate::herdr::types::{Agent, AgentStatus, Snapshot, Workspace};
use crate::native::headers;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Group,
    Space,
    Worktree,
    Agent,
}

/// Color hint; the TUI maps names and statuses to actual colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tint {
    /// Palette name (`blue`, …, or `grey` for Ungrouped).
    Group(String),
    Status(AgentStatus),
    Grey,
    Plain,
}

/// What `Enter` jumps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Workspace(String),
    Pane(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    pub depth: u8,
    pub text: String,
    pub tint: Tint,
    pub is_focused: bool,
    pub collapsible: bool,
    pub collapsed: bool,
    /// Fold key (`g:<name>` / `s:<path>`); agents have none.
    pub key: Option<String>,
    pub target: Option<Target>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub row: Row,
    pub children: Vec<Node>,
}

/// Group folds are the sidebar's (shared with Herdr's Spaces list); Space folds live in the popup.
#[derive(Debug, Clone, Default)]
pub struct Folds {
    pub sidebar: Sidebar,
    pub spaces: BTreeSet<String>,
}

impl Folds {
    pub fn is_collapsed(&self, key: &str) -> bool {
        match key.strip_prefix("g:") {
            Some(group) => self.sidebar.is_folded(group),
            None => self.spaces.contains(key),
        }
    }
}

pub fn group_key(name: &str) -> String {
    format!("g:{name}")
}

pub fn space_key(path_key: &str) -> String {
    format!("s:{path_key}")
}

pub fn build(snapshot: &Snapshot, groups: &Groups, folds: &Folds) -> Vec<Node> {
    let ctx = Ctx::new(snapshot, folds);
    let children = ctx.children_by_primary();
    let mut members: BTreeMap<&str, Vec<&Workspace>> = BTreeMap::new();
    for ws in ctx.projects() {
        if ws.is_linked_worktree() && ctx.primary_of(ws).is_some() {
            continue;
        }
        let group = groups.group_of(&PathKey::for_workspace(ws, &snapshot.panes)).unwrap_or(UNGROUPED);
        members.entry(group).or_default().push(ws);
    }
    let order = groups.groups.iter().map(|g| (g.name.as_str(), g.color.as_str())).chain([(UNGROUPED, "grey")]);
    order
        .filter_map(|(name, color)| {
            let spaces = members.get(name)?;
            let nodes = spaces.iter().map(|ws| ctx.space_node(ws, children.get(ws.workspace_id.as_str()))).collect();
            Some(ctx.group_node(name, color, nodes))
        })
        .collect()
}

pub fn visible(nodes: &[Node]) -> Vec<Row> {
    let mut out = Vec::new();
    for node in nodes {
        out.push(node.row.clone());
        if !node.row.collapsed {
            out.extend(visible(&node.children));
        }
    }
    out
}

struct Ctx<'a> {
    snapshot: &'a Snapshot,
    folds: &'a Folds,
    /// workspace_id → its agents, sorted by status priority (stable within a status).
    agents: BTreeMap<&'a str, Vec<&'a Agent>>,
    /// repo_key → primary workspace_id
    primaries: BTreeMap<&'a str, &'a str>,
}

impl<'a> Ctx<'a> {
    fn new(snapshot: &'a Snapshot, folds: &'a Folds) -> Self {
        let mut agents: BTreeMap<&str, Vec<&Agent>> = BTreeMap::new();
        for a in &snapshot.agents {
            agents.entry(a.workspace_id.as_str()).or_default().push(a);
        }
        for list in agents.values_mut() {
            list.sort_by_key(|a| a.agent_status.priority());
        }
        let mut primaries = BTreeMap::new();
        for ws in &snapshot.workspaces {
            if let Some(wt) = ws.worktree.as_ref().filter(|wt| !wt.is_linked_worktree) {
                primaries.entry(wt.repo_key.as_str()).or_insert(ws.workspace_id.as_str());
            }
        }
        Self { snapshot, folds, agents, primaries }
    }

    /// Header workspaces are sidebar furniture, not Spaces.
    fn projects(&self) -> impl Iterator<Item = &'a Workspace> {
        self.snapshot.workspaces.iter().filter(|ws| headers::group_of(ws).is_none())
    }

    fn primary_of(&self, ws: &Workspace) -> Option<&'a str> {
        let wt = ws.worktree.as_ref().filter(|wt| wt.is_linked_worktree)?;
        self.primaries.get(wt.repo_key.as_str()).copied()
    }

    fn children_by_primary(&self) -> BTreeMap<&'a str, Vec<&'a Workspace>> {
        let mut map: BTreeMap<&str, Vec<&Workspace>> = BTreeMap::new();
        for ws in self.projects() {
            if let Some(parent) = self.primary_of(ws) {
                map.entry(parent).or_default().push(ws);
            }
        }
        map
    }

    fn own_agents(&self, ws: &Workspace) -> &[&'a Agent] {
        self.agents.get(ws.workspace_id.as_str()).map(Vec::as_slice).unwrap_or_default()
    }

    fn own_state(&self, ws: &Workspace) -> Option<AgentStatus> {
        self.own_agents(ws).first().map(|a| a.agent_status)
    }

    fn group_node(&self, name: &str, color: &str, spaces: Vec<Node>) -> Node {
        let count = |status| spaces.iter().map(|n| count_agents(n, status)).sum::<usize>();
        let key = group_key(name);
        let collapsed = self.folds.is_collapsed(&key);
        let mut text = format!("{} {name} ·{}", if collapsed { '▸' } else { '▾' }, spaces.len());
        for status in [AgentStatus::Working, AgentStatus::Blocked] {
            let n = count(status);
            if n > 0 {
                text.push_str(&format!(" {}{n}", status.glyph()));
            }
        }
        let row = Row {
            kind: Kind::Group,
            depth: 0,
            text,
            tint: Tint::Group(color.to_owned()),
            is_focused: false,
            collapsible: name != UNGROUPED,
            collapsed,
            key: Some(key),
            target: None,
        };
        Node { row, children: spaces }
    }

    /// Top-level entry of a group: a primary Space (with nested worktrees) or an orphan worktree.
    fn space_node(&self, ws: &'a Workspace, worktrees: Option<&Vec<&'a Workspace>>) -> Node {
        let state = self.own_state(ws);
        let icon = state.map(AgentStatus::glyph).unwrap_or(' ');
        let (kind, text, tint, key) = match ws.worktree.as_ref().filter(|wt| wt.is_linked_worktree) {
            Some(wt) => (
                Kind::Worktree,
                format!("  {icon} ⎇ {} ({})", ws.label, wt.repo_name),
                state.map(Tint::Status).unwrap_or(Tint::Grey),
                worktree_key(ws),
            ),
            None => (
                Kind::Space,
                format!("  {icon} {}", ws.label),
                state.map(Tint::Status).unwrap_or(Tint::Plain),
                space_key(PathKey::for_workspace(ws, &self.snapshot.panes).as_str()),
            ),
        };
        let mut children: Vec<Node> = self.own_agents(ws).iter().map(|a| self.agent_node(a, 2)).collect();
        let kids = worktrees.map(Vec::as_slice).unwrap_or_default();
        for (i, kid) in kids.iter().enumerate() {
            children.push(self.worktree_node(kid, i + 1 == kids.len()));
        }
        self.node(kind, 1, text, tint, ws, key, children)
    }

    fn worktree_node(&self, ws: &'a Workspace, is_last: bool) -> Node {
        let state = self.own_state(ws);
        let icon = state.map(AgentStatus::glyph).unwrap_or('·');
        let tee = if is_last { "└─" } else { "├─" };
        let text = format!("    {tee} {icon} {}", ws.label);
        let tint = state.map(Tint::Status).unwrap_or(Tint::Grey);
        let children = self.own_agents(ws).iter().map(|a| self.agent_node(a, 3)).collect();
        self.node(Kind::Worktree, 2, text, tint, ws, worktree_key(ws), children)
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        &self,
        kind: Kind,
        depth: u8,
        text: String,
        tint: Tint,
        ws: &Workspace,
        key: String,
        children: Vec<Node>,
    ) -> Node {
        let row = Row {
            kind,
            depth,
            text,
            tint,
            is_focused: self.snapshot.focused_workspace_id.as_deref() == Some(ws.workspace_id.as_str()),
            collapsible: !children.is_empty(),
            collapsed: self.folds.is_collapsed(&key),
            key: Some(key),
            target: Some(Target::Workspace(ws.workspace_id.clone())),
        };
        Node { row, children }
    }

    fn agent_node(&self, a: &Agent, depth: u8) -> Node {
        let title = a.terminal_title_stripped.as_deref().unwrap_or_default();
        let body: Vec<&str> = [a.agent.as_str(), title].into_iter().filter(|p| !p.is_empty()).collect();
        let indent = " ".repeat(4 * (depth as usize - 1));
        let row = Row {
            kind: Kind::Agent,
            depth,
            text: format!("{indent}{} {}", a.agent_status.glyph(), body.join("  ")),
            tint: Tint::Status(a.agent_status),
            is_focused: self.snapshot.focused_pane_id.as_deref() == Some(a.pane_id.as_str()),
            collapsible: false,
            collapsed: false,
            key: None,
            target: Some(Target::Pane(a.pane_id.clone())),
        };
        Node { row, children: Vec::new() }
    }
}

/// Worktree rows fold under their own checkout path, so an orphan never collides with its primary.
fn worktree_key(ws: &Workspace) -> String {
    let checkout = ws.worktree.as_ref().map(|wt| wt.checkout_path.as_str()).unwrap_or_default();
    space_key(&realpath(checkout))
}

/// Counts agent rows by status; the agent row's tint is its status.
fn count_agents(node: &Node, status: AgentStatus) -> usize {
    let own = usize::from(node.row.kind == Kind::Agent && node.row.tint == Tint::Status(status));
    own + node.children.iter().map(|c| count_agents(c, status)).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::types::Worktree;

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

    fn snapshot() -> Snapshot {
        Snapshot {
            focused_workspace_id: Some("a".into()),
            workspaces: vec![
                ws("h", "⟦Work⟧", "/h", false),
                ws("a", "api", "/a", false),
                ws("wt", "feat", "/a", true),
                ws("u", "notes", "/u", false),
            ],
            agents: vec![Agent {
                pane_id: "a:p1".into(),
                workspace_id: "a".into(),
                agent: "claude".into(),
                agent_status: AgentStatus::Working,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.text.clone()).collect()
    }

    #[test]
    fn groups_then_ungrouped_without_header_workspaces() {
        let groups = Groups::default().create("Work").unwrap().move_to(&realpath("/a"), Some("Work"));
        let rows = visible(&build(&snapshot(), &groups, &Folds::default()));
        assert_eq!(
            texts(&rows),
            ["▾ Work ·1 ●1", "  ● api", "    ● claude", "    └─ · feat", "▾ Ungrouped ·1", "    notes"]
        );
        assert!(rows[1].is_focused);
        assert!(!rows[4].collapsible, "Ungrouped cannot fold");
    }

    #[test]
    fn sidebar_folds_hide_group_members() {
        let groups = Groups::default().create("Work").unwrap().move_to(&realpath("/a"), Some("Work"));
        let folds =
            Folds { sidebar: Sidebar { collapsed: vec!["Work".into()], ..Default::default() }, ..Default::default() };
        let rows = visible(&build(&snapshot(), &groups, &folds));
        assert_eq!(texts(&rows)[0], "▸ Work ·1 ●1");
        assert_eq!(rows.len(), 3);
    }
}
