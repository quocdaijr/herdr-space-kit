//! One empty "header" workspace per group, so the group label never rides on a project.
//! A header is labelled `⟦Group⟧`, opened in an empty folder under the state dir, and found again
//! by label after a server restart (ids and tokens do not survive one).
use crate::herdr::client::{Api, Result};
use crate::herdr::types::Workspace;
use serde_json::json;
use std::path::{Path, PathBuf};

pub fn label(group: &str) -> String {
    format!("⟦{group}⟧")
}

pub fn group_of(ws: &Workspace) -> Option<&str> {
    ws.label.strip_prefix('⟦')?.strip_suffix('⟧').filter(|g| !g.is_empty())
}

pub struct Split<'a> {
    pub projects: Vec<&'a Workspace>,
    /// First header workspace per group, in sidebar order.
    pub headers: Vec<(&'a str, &'a Workspace)>,
    pub duplicates: Vec<&'a Workspace>,
}

impl<'a> Split<'a> {
    pub fn of(workspaces: &'a [Workspace]) -> Self {
        let mut split = Split { projects: Vec::new(), headers: Vec::new(), duplicates: Vec::new() };
        for ws in workspaces {
            match group_of(ws) {
                None => split.projects.push(ws),
                Some(g) if split.headers.iter().any(|(h, _)| *h == g) => split.duplicates.push(ws),
                Some(g) => split.headers.push((g, ws)),
            }
        }
        split
    }

    pub fn header_id(&self, group: &str) -> Option<&'a str> {
        self.headers.iter().find(|(g, _)| *g == group).map(|(_, w)| w.workspace_id.as_str())
    }

    /// Groups that need a header created, and header workspace ids to close.
    pub fn plan(&self, wanted: &[&str]) -> (Vec<String>, Vec<String>) {
        let create = wanted.iter().filter(|g| self.header_id(g).is_none()).map(|g| g.to_string()).collect();
        let close = self
            .headers
            .iter()
            .filter(|(g, _)| !wanted.contains(g))
            .map(|(_, w)| w)
            .chain(&self.duplicates)
            .map(|w| w.workspace_id.clone())
            .collect();
        (create, close)
    }
}

/// `<state_dir>/headers/<name>` with anything but `[A-Za-z0-9._-]` turned into `_`.
pub fn folder(state_dir: &Path, group: &str) -> PathBuf {
    let safe: String = group.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    let safe = safe.trim_matches('_');
    state_dir.join("headers").join(if safe.is_empty() { "group" } else { safe })
}

/// Returns true when anything was created or closed (the caller then re-reads the snapshot).
pub fn apply(api: &dyn Api, state_dir: &Path, create: &[String], close: &[String]) -> Result<bool> {
    for id in close {
        api.call("workspace.close", json!({"workspace_id": id}))?;
    }
    for group in create {
        let dir = folder(state_dir, group);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        api.call("workspace.create", json!({"label": label(group), "cwd": dir.to_string_lossy(), "focus": false}))?;
    }
    Ok(!create.is_empty() || !close.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: &str, label: &str) -> Workspace {
        Workspace { workspace_id: id.into(), label: label.into(), ..Default::default() }
    }

    #[test]
    fn labels_round_trip_and_empty_brackets_are_projects() {
        assert_eq!(group_of(&ws("h", &label("MADP"))), Some("MADP"));
        assert_eq!(group_of(&ws("p", "web-app")), None);
        assert_eq!(group_of(&ws("p", "⟦⟧")), None);
    }

    #[test]
    fn plan_creates_missing_and_closes_unwanted_and_duplicates() {
        let all = [ws("p1", "api"), ws("h1", "⟦A⟧"), ws("h2", "⟦A⟧"), ws("h9", "⟦Gone⟧")];
        let split = Split::of(&all);
        assert_eq!(split.projects.len(), 1);
        assert_eq!(split.header_id("A"), Some("h1"));
        let (create, close) = split.plan(&["A", "B"]);
        assert_eq!(create, ["B"]);
        assert_eq!(close, ["h9", "h2"]);
    }

    #[test]
    fn folder_is_filesystem_safe() {
        assert_eq!(folder(Path::new("/s"), "My Group/1"), PathBuf::from("/s/headers/My_Group_1"));
        assert_eq!(folder(Path::new("/s"), "数"), PathBuf::from("/s/headers/group"));
    }
}
