//! Serde types for the parts of `session.snapshot` the tree uses.
//! Every optional field defaults; unknown fields are ignored (serde default).
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
    #[serde(default)]
    pub focused_tab_id: Option<String>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub worktree: Option<Worktree>,
}

impl Workspace {
    pub fn is_linked_worktree(&self) -> bool {
        self.worktree.as_ref().is_some_and(|w| w.is_linked_worktree)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Worktree {
    #[serde(default)]
    pub repo_key: String,
    #[serde(default)]
    pub repo_root: String,
    #[serde(default)]
    pub repo_name: String,
    #[serde(default)]
    pub checkout_path: String,
    #[serde(default)]
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Agent {
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub agent_status: AgentStatus,
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    /// Stays the same across `pane.move`, unlike `pane_id`.
    #[serde(default)]
    pub terminal_id: Option<String>,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(default)]
    pub restore_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Blocked,
    Working,
    Done,
    Idle,
    #[default]
    #[serde(other)]
    Unknown,
}

impl AgentStatus {
    /// 0 = most urgent. Declaration order is the priority order.
    pub fn priority(self) -> u8 {
        self as u8
    }

    pub fn glyph(self) -> char {
        match self {
            Self::Blocked => '?',
            Self::Working => '●',
            Self::Done => '✓',
            Self::Idle => '○',
            Self::Unknown => '·',
        }
    }
}
