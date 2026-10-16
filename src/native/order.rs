//! Sidebar layout: groups in groups.json order (each led by its header workspace), Spaces in
//! snapshot order, linked worktrees right after their primary, Ungrouped last.
use crate::herdr::types::{Pane, Workspace};
use crate::model::groups::{Groups, PathKey};
use std::collections::BTreeMap;

/// A top-level row of a section: a primary Space with its linked worktrees, or an orphan worktree.
pub struct Entry<'a> {
    pub ws: &'a Workspace,
    pub worktrees: Vec<&'a Workspace>,
}

pub struct Section<'a> {
    /// None = Ungrouped.
    pub group: Option<&'a str>,
    pub entries: Vec<Entry<'a>>,
}

/// Non-empty sections in display order.
pub fn sections<'a>(projects: &[&'a Workspace], panes: &[Pane], groups: &'a Groups) -> Vec<Section<'a>> {
    let mut primaries: BTreeMap<&str, &str> = BTreeMap::new();
    for ws in projects {
        if let Some(wt) = ws.worktree.as_ref().filter(|wt| !wt.is_linked_worktree) {
            primaries.entry(wt.repo_key.as_str()).or_insert(ws.workspace_id.as_str());
        }
    }
    let parent_of = |ws: &Workspace| {
        let wt = ws.worktree.as_ref().filter(|wt| wt.is_linked_worktree)?;
        primaries.get(wt.repo_key.as_str()).copied()
    };
    let mut kids: BTreeMap<&str, Vec<&Workspace>> = BTreeMap::new();
    let mut members: BTreeMap<Option<&str>, Vec<&Workspace>> = BTreeMap::new();
    for ws in projects {
        if let Some(parent) = parent_of(ws) {
            kids.entry(parent).or_default().push(ws);
            continue;
        }
        let group = groups.group_of(&PathKey::for_workspace(ws, panes));
        members.entry(group).or_default().push(ws);
    }
    let names = groups.groups.iter().map(|g| Some(g.name.as_str())).chain([None]);
    names
        .filter_map(|group| {
            let entries = members
                .remove(&group)?
                .into_iter()
                .map(|ws| Entry { ws, worktrees: kids.remove(ws.workspace_id.as_str()).unwrap_or_default() })
                .collect();
            Some(Section { group, entries })
        })
        .collect()
}

/// Every workspace id in sidebar order; `header_of` gives a group's header workspace.
pub fn desired_order<'a>(sections: &[Section<'a>], header_of: impl Fn(&str) -> Option<&'a str>) -> Vec<String> {
    let mut order = Vec::new();
    for section in sections {
        if let Some(header) = section.group.and_then(&header_of) {
            order.push(header.to_owned());
        }
        for entry in &section.entries {
            order.push(entry.ws.workspace_id.clone());
            order.extend(entry.worktrees.iter().map(|w| w.workspace_id.clone()));
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::types::Worktree;

    fn ws(id: &str, root: &str, linked: bool) -> Workspace {
        let worktree = Worktree { repo_root: root.into(), repo_key: format!("{root}/.git"), is_linked_worktree: linked, ..Default::default() };
        Workspace { workspace_id: id.into(), label: id.into(), worktree: Some(worktree), ..Default::default() }
    }

    #[test]
    fn groups_are_contiguous_worktrees_follow_parents_ungrouped_last() {
        let all = [ws("u", "/u", false), ws("a", "/a", false), ws("b", "/b", false), ws("a-wt", "/a", true), ws("c", "/c", false)];
        let projects: Vec<&Workspace> = all.iter().collect();
        let groups = Groups::default().create("Work").unwrap().create("Home").unwrap();
        let groups = groups.move_to("/c", Some("Work")).move_to("/a", Some("Work")).move_to("/b", Some("Home"));
        let sections = sections(&projects, &[], &groups);
        assert_eq!(sections.iter().map(|s| s.group).collect::<Vec<_>>(), [Some("Work"), Some("Home"), None]);
        let order = desired_order(&sections, |g| (g == "Work").then_some("hW"));
        assert_eq!(order, ["hW", "a", "a-wt", "c", "b", "u"]);
    }
}
