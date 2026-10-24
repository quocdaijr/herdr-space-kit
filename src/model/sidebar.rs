//! sidebar.json: which groups are folded, plus the header ids that tell a header click.
use super::groups::Groups;
use super::write_atomic;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use std::path::Path;

const FILE: &str = "sidebar.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidebar {
    /// Folded group names, in group order.
    #[serde(default)]
    pub collapsed: Vec<String>,
    /// Header workspace ids, so `workspace.focused` can tell a header click cheaply.
    #[serde(default)]
    pub header_ids: Vec<String>,
    /// Setting (off by default): a header click also unfolds its group. Off, only `Space` in the
    /// overview folds and unfolds. Toggled with `c` in the overview.
    #[serde(default)]
    pub unfold_on_click: bool,
}

impl Sidebar {
    /// Missing or malformed file → nothing folded.
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read_to_string(state_dir.join(FILE))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        write_atomic(&state_dir.join(FILE), &(json + "\n"))
    }

    pub(super) fn from_v1(session: &Value) -> Self {
        let strings =
            |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        Self {
            collapsed: strings(&session["collapsed"]),
            header_ids: strings(&session["header_ids"]),
            ..Self::default()
        }
    }

    pub fn is_folded(&self, group: &str) -> bool {
        self.collapsed.iter().any(|g| g == group)
    }

    /// Flip one group; the result keeps group order.
    pub fn toggle(&self, group: &str, groups: &Groups) -> Self {
        let fold = !self.is_folded(group);
        let collapsed = groups
            .groups
            .iter()
            .map(|g| g.name.clone())
            .filter(|g| if g == group { fold } else { self.is_folded(g) })
            .collect();
        Self { collapsed, ..self.clone() }
    }

    /// Drop folds of groups that no longer exist (and follow a rename).
    pub fn normalized(&self, groups: &Groups) -> Self {
        let collapsed = groups.groups.iter().map(|g| g.name.clone()).filter(|g| self.is_folded(g)).collect();
        Self { collapsed, ..self.clone() }
    }

    pub fn renamed(&self, old: &str, new: &str) -> Self {
        let collapsed = self.collapsed.iter().map(|g| if g == old { new.to_owned() } else { g.clone() }).collect();
        Self { collapsed, ..self.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_and_normalize_follow_group_order() {
        let groups = Groups::default().create("a").unwrap().create("b").unwrap();
        let folded = Sidebar::default().toggle("b", &groups).toggle("a", &groups);
        assert_eq!(folded.collapsed, ["a", "b"]);
        assert_eq!(folded.toggle("a", &groups).collapsed, ["b"]);
        let stale = Sidebar { collapsed: vec!["gone".into(), "b".into()], ..Default::default() };
        assert_eq!(stale.normalized(&groups).collapsed, ["b"]);
        assert_eq!(folded.renamed("a", "z").collapsed, ["z", "b"]);
    }
}
