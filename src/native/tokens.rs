//! `group_header` / `group_row` / `group_branch` per workspace. A whole row is one token, so Herdr
//! draws no `·` separators; Herdr indents rows after an entry's first by two columns, hence the
//! braille blanks. A primary workspace with worktrees or agents shows its main checkout as a
//! `[primary]` branch row (`group_branch`), so the repo row itself carries no agent counts and
//! every workspace reads the same way.
use super::order::Section;
use crate::herdr::types::AgentStatus;
use crate::model::sidebar::Sidebar;
use crate::text::{fit_row, rollup, state_counts};
use std::collections::BTreeMap;

pub type Tokens = BTreeMap<String, Option<String>>;

/// Braille blanks: Herdr does not trim them like spaces.
const PAD: &str = "⠀⠀";
/// Prefix of the main-checkout row. Herdr indents an entry's second row by two columns; three
/// more line the label up with the worktree names after Herdr's own (dim) `├─` connectors. No
/// connector of our own: a cell has one color, so it could never match Herdr's.
const BRANCH: &str = "⠀⠀⠀";
/// Leads a row that stays visible in a folded group (the focused one, or one blocked on you).
const PEEK: &str = "▸ ";
/// Ends the focused row of a folded group: it shows because it is open, not because of the fold.
const OPEN: &str = "  (open)";
/// Marks the main checkout among the repo's worktrees.
const PRIMARY: &str = "[primary]";
const RULE: &str = "━━━━━━━━━━";

pub struct Layout<'a> {
    pub sections: &'a [Section<'a>],
    /// group → its header workspace id
    pub headers: &'a BTreeMap<&'a str, &'a str>,
    /// workspace id → its agents' states
    pub statuses: &'a BTreeMap<&'a str, Vec<AgentStatus>>,
    /// primary workspace id → the branch its main checkout is on
    pub branches: &'a BTreeMap<&'a str, String>,
    pub sidebar: &'a Sidebar,
    pub focused: Option<&'a str>,
    pub width: usize,
}

fn row_tokens(header: Option<String>, row: Option<String>, branch: Option<String>) -> Tokens {
    BTreeMap::from([
        ("group_header".to_owned(), header),
        ("group_row".to_owned(), row),
        ("group_branch".to_owned(), branch),
    ])
}

pub fn tokens(layout: &Layout) -> BTreeMap<String, Tokens> {
    let states = |id: &str| layout.statuses.get(id).map(Vec::as_slice).unwrap_or_default();
    let row = |prefix: &str, label: &str, id: &str, note: &str| {
        fit_row(prefix, label, &format!("{}{note}", state_counts(states(id))), layout.width)
    };
    let mut out = BTreeMap::new();
    for section in layout.sections {
        let n = section.entries.len();
        let folded = section.group.is_some_and(|g| layout.sidebar.is_folded(g));
        let header_ws = section.group.and_then(|g| layout.headers.get(g).copied());
        let header = section.group.map(|g| {
            let family = section.entries.iter().flat_map(|e| std::iter::once(e.ws).chain(e.worktrees.iter().copied()));
            let all: Vec<AgentStatus> = family.flat_map(|w| states(&w.workspace_id).to_vec()).collect();
            let title = if folded { format!("▸ {g}") } else { g.to_owned() };
            let counts = if folded { rollup(&all) } else { String::new() };
            format!("━━ {title} · {n}{counts} {RULE}")
        });
        if let (Some(id), Some(text)) = (header_ws, &header) {
            out.insert(id.to_owned(), row_tokens(Some(text.clone()), None, None));
        }
        for (i, entry) in section.entries.iter().enumerate() {
            let tree = match (section.group, i + 1 == n) {
                (None, _) => "",
                (Some(_), true) => "└─ ",
                (Some(_), false) => "├─ ",
            };
            // Without its header workspace (not created yet) the first entry carries the header.
            let carries_header = header_ws.is_none() && i == 0;
            let pad = if section.group.is_some() && header_ws.is_none() && i > 0 { PAD } else { "" };
            // Herdr nests linked worktrees under their repo itself, so worktree rows need no pad.
            let rows = std::iter::once((entry.ws, format!("{pad}{tree}"))).chain(entry.worktrees.iter().map(|w| (*w, String::new())));
            // In a folded group the focused workspace keeps its whole family showing: the repo,
            // its main checkout and every worktree (Herdr still hides worktrees it folded itself).
            let family_open = std::iter::once(entry.ws).chain(entry.worktrees.iter().copied()).any(|w| layout.focused == Some(w.workspace_id.as_str()));
            for (ws, prefix) in rows {
                let id = ws.workspace_id.as_str();
                let focused = layout.focused == Some(id);
                let visible = !folded || family_open || states(id).contains(&AgentStatus::Blocked);
                let is_primary = ws.workspace_id == entry.ws.workspace_id;
                // A row left showing in a folded group says so, so it does not read as unfolded.
                let peeking = folded && visible;
                let prefix = match (peeking, is_primary) {
                    (false, _) => prefix,
                    (true, true) => format!("{pad}{PEEK}"),
                    (true, false) => PEEK.to_owned(),
                };
                let note = if peeking && focused { OPEN } else { "" };
                let has_branch_row = is_primary && (!entry.worktrees.is_empty() || !states(id).is_empty());
                let (text, branch) = match has_branch_row {
                    // The main checkout reads as one more branch, like the worktrees.
                    true => {
                        let label = match layout.branches.get(id) {
                            Some(name) => format!("{PRIMARY} - {name}"),
                            None => PRIMARY.to_owned(),
                        };
                        let room = layout.width.saturating_sub(2 + crate::text::width(BRANCH));
                        let branch_row = format!("{BRANCH}{}", fit_row("", &label, &state_counts(states(id)), room));
                        (visible.then(|| fit_row(&prefix, &ws.label, note, layout.width)), visible.then_some(branch_row))
                    }
                    false => (visible.then(|| row(&prefix, &ws.label, id, note)), None),
                };
                let header = (carries_header && ws.workspace_id == entry.ws.workspace_id).then(|| header.clone()).flatten();
                out.insert(id.to_owned(), row_tokens(header, text, branch));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::types::{Workspace, Worktree};
    use crate::model::groups::Groups;
    use crate::native::order::sections;
    use AgentStatus::*;

    fn ws(id: &str, root: &str, linked: bool) -> Workspace {
        let worktree = Worktree { repo_root: root.into(), repo_key: format!("{root}/.git"), is_linked_worktree: linked, ..Default::default() };
        Workspace { workspace_id: id.into(), label: format!("{id}-label"), worktree: Some(worktree), ..Default::default() }
    }

    struct Case {
        all: Vec<Workspace>,
        groups: Groups,
        sidebar: Sidebar,
        statuses: BTreeMap<&'static str, Vec<AgentStatus>>,
        focused: Option<&'static str>,
        with_header: bool,
    }

    impl Case {
        fn new() -> Self {
            let all = vec![ws("a", "/a", false), ws("a-wt", "/a", true), ws("b", "/b", false), ws("c", "/c", false), ws("u", "/u", false)];
            let groups = Groups::default().create("Work").unwrap().move_to("/a", Some("Work")).move_to("/b", Some("Work")).move_to("/c", Some("Work"));
            Self { all, groups, sidebar: Sidebar::default(), statuses: BTreeMap::new(), focused: None, with_header: true }
        }

        fn run(&self) -> BTreeMap<String, Tokens> {
            let projects: Vec<&Workspace> = self.all.iter().collect();
            let sections = sections(&projects, &[], &self.groups);
            let headers: BTreeMap<&str, &str> = if self.with_header { [("Work", "h")].into() } else { BTreeMap::new() };
            let branches = BTreeMap::from([("a", "develop".to_owned())]);
            tokens(&Layout { sections: &sections, headers: &headers, statuses: &self.statuses, branches: &branches, sidebar: &self.sidebar, focused: self.focused, width: 30 })
        }
    }

    fn get<'a>(t: &'a BTreeMap<String, Tokens>, id: &str, name: &str) -> Option<&'a str> {
        t[id][name].as_deref()
    }

    #[test]
    fn header_workspace_carries_the_header_and_members_draw_a_tree() {
        let mut case = Case::new();
        case.statuses = [("b", vec![Working, Working, Idle]), ("a", vec![Idle, Idle]), ("a-wt", vec![Working])].into();
        let t = case.run();
        assert_eq!(get(&t, "h", "group_header"), Some("━━ Work · 3 ━━━━━━━━━━"));
        assert_eq!(get(&t, "h", "group_row"), None);
        assert_eq!(get(&t, "a", "group_row"), Some("├─ a-label"), "a repo with worktrees shows no counts itself");
        assert_eq!(get(&t, "a", "group_branch"), Some("⠀⠀⠀[primary] - develop  ○2"), "its main checkout does");
        assert_eq!(get(&t, "a-wt", "group_row"), Some("a-wt-label  ●1"));
        assert_eq!(get(&t, "a-wt", "group_branch"), None);
        assert_eq!(get(&t, "b", "group_row"), Some("├─ b-label"), "agents move to the branch row");
        assert_eq!(get(&t, "b", "group_branch"), Some("⠀⠀⠀[primary]  ●2 ○1"), "unknown branch: just the marker");
        assert_eq!(get(&t, "c", "group_branch"), None, "no worktrees and no agents: one row");
        assert_eq!(get(&t, "c", "group_row"), Some("└─ c-label"));
        assert_eq!(get(&t, "u", "group_row"), Some("u-label"), "ungrouped rows are plain");
        assert_eq!(get(&t, "a", "group_header"), None);
    }

    #[test]
    fn folded_group_keeps_focused_and_blocked_rows_and_rolls_up_counts() {
        let mut case = Case::new();
        case.sidebar = Sidebar { collapsed: vec!["Work".into()], ..Default::default() };
        case.statuses = [("a-wt", vec![Blocked]), ("c", vec![Working])].into();
        case.focused = Some("b");
        let t = case.run();
        assert_eq!(get(&t, "h", "group_header"), Some("━━ ▸ Work · 3 ●1 ?1 ━━━━━━━━━━"));
        assert_eq!(get(&t, "a", "group_row"), None);
        assert_eq!(get(&t, "a", "group_branch"), None);
        assert_eq!(get(&t, "a-wt", "group_row"), Some("▸ a-wt-label  ?1"), "blocked rows stay, marked as peeking");
        assert_eq!(get(&t, "b", "group_row"), Some("▸ b-label  (open)"), "the focused row says why it shows");
        assert_eq!(get(&t, "c", "group_row"), None);

        // Focusing a worktree keeps its repo, main checkout and sibling worktrees showing.
        case.focused = Some("a-wt");
        let t = case.run();
        assert_eq!(get(&t, "a", "group_row"), Some("▸ a-label"));
        assert_eq!(get(&t, "a", "group_branch"), Some("⠀⠀⠀[primary] - develop"));
        assert_eq!(get(&t, "a-wt", "group_row"), Some("▸ a-wt-label  ?1  (open)"));
        assert_eq!(get(&t, "b", "group_row"), None, "other families stay folded");
    }

    #[test]
    fn without_a_header_workspace_the_first_member_carries_the_header() {
        let mut case = Case::new();
        case.with_header = false;
        let t = case.run();
        assert_eq!(get(&t, "a", "group_header"), Some("━━ Work · 3 ━━━━━━━━━━"));
        assert_eq!(get(&t, "a", "group_row"), Some("├─ a-label"));
        assert_eq!(get(&t, "b", "group_row"), Some("⠀⠀├─ b-label"));
    }
}
