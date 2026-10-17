//! Serde types for the parts of `session.snapshot` space-kit reads.
//! Every optional field defaults; unknown fields are ignored (serde default).
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
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
    pub terminal_title_stripped: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
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

    /// The glyph Herdr draws for this status under the user's `ui.status_indicators`.
    pub fn glyph(self) -> char {
        self.glyph_in(Indicators::current())
    }

    /// Herdr's `status_icon` table (src/client/shell.rs).
    pub fn glyph_in(self, style: Indicators) -> char {
        match (style, self) {
            (Indicators::Dots, Self::Blocked | Self::Working | Self::Done) => '●',
            (Indicators::Symbols, Self::Blocked) => '×',
            (Indicators::Symbols, Self::Working) => '◐',
            (Indicators::Symbols, Self::Done) => '✓',
            (_, Self::Idle) => '○',
            (_, Self::Unknown) => '·',
        }
    }
}

/// Herdr's `ui.status_indicators`: colour dots (the default) or distinct symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicators {
    Dots,
    Symbols,
}

impl Indicators {
    /// Read once per process from Herdr's config.
    pub fn current() -> Self {
        static STYLE: std::sync::OnceLock<Indicators> = std::sync::OnceLock::new();
        *STYLE.get_or_init(|| crate::model::herdr_config().map_or(Self::Dots, |t| Self::from_config(&t)))
    }

    pub fn from_config(config: &toml::Table) -> Self {
        match config.get("ui").and_then(|ui| ui.get("status_indicators")?.as_str()) {
            Some("symbols") => Self::Symbols,
            _ => Self::Dots,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_have_a_glyph_and_a_priority() {
        let all =
            [AgentStatus::Blocked, AgentStatus::Working, AgentStatus::Done, AgentStatus::Idle, AgentStatus::Unknown];
        assert_eq!(all.map(|s| s.glyph_in(Indicators::Symbols)), ['×', '◐', '✓', '○', '·']);
        assert_eq!(all.map(|s| s.glyph_in(Indicators::Dots)), ['●', '●', '●', '○', '·']);
        assert_eq!(
            all.map(AgentStatus::glyph),
            all.map(|s| s.glyph_in(Indicators::Dots)),
            "no config: Herdr's default"
        );
        assert_eq!(all.map(AgentStatus::priority), [0, 1, 2, 3, 4], "most urgent first");
        let unknown: AgentStatus = serde_json::from_str("\"thinking\"").unwrap();
        assert_eq!(unknown, AgentStatus::Unknown, "new states from Herdr read as unknown");
    }

    #[test]
    fn indicators_follow_herdr_config() {
        let parse = |t: &str| Indicators::from_config(&t.parse().unwrap());
        assert_eq!(parse("[ui]\nstatus_indicators = \"symbols\""), Indicators::Symbols);
        assert_eq!(parse("[ui]\nstatus_indicators = \"dots\""), Indicators::Dots);
        assert_eq!(parse("[ui]\nstatus_indicators = \"other\""), Indicators::Dots);
        assert_eq!(parse("[ui]\nsidebar_width = 30"), Indicators::Dots);
    }
}
