//! groups.json v2: ordered, colored groups and path-keyed assignments. Every edit returns a new
//! value. A v1 file (the earlier workspace-id-keyed format) is imported once on first load.
use super::sidebar::Sidebar;
use super::{realpath, write_atomic};
use crate::herdr::types::{Pane, Snapshot, Workspace};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

pub const FILE: &str = "groups.json";
const V1_BACKUP: &str = "groups.v1.json";
pub const VERSION: u32 = 2;
pub const UNGROUPED: &str = "Ungrouped";
/// A new group takes the first color no existing group uses.
pub const PALETTE: [&str; 7] = ["blue", "green", "peach", "mauve", "yellow", "teal", "red"];

/// Identity of a Space across Herdr restarts: realpath of its repo root, else of its first
/// pane's cwd, else `label:<name>`. Linked worktrees share their repo's root, so they inherit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathKey(String);

impl PathKey {
    pub fn for_workspace(ws: &Workspace, panes: &[Pane]) -> Self {
        if let Some(root) = ws.worktree.as_ref().map(|w| w.repo_root.as_str()).filter(|r| !r.is_empty()) {
            return Self(realpath(root));
        }
        let cwd = panes
            .iter()
            .filter(|p| p.workspace_id == ws.workspace_id)
            .find_map(|p| p.cwd.as_deref().filter(|c| !c.is_empty()));
        match cwd {
            Some(cwd) => Self(realpath(cwd)),
            None => Self(format!("label:{}", if ws.label.is_empty() { &ws.workspace_id } else { &ws.label })),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Only real paths can be checked on disk; `label:` keys are never cleaned.
    pub fn as_path(&self) -> Option<&Path> {
        self.0.starts_with('/').then(|| Path::new(&self.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Groups {
    pub version: u32,
    #[serde(default)]
    pub groups: Vec<Group>,
    /// path key → group name
    #[serde(default)]
    pub assignments: BTreeMap<String, String>,
}

impl Default for Groups {
    fn default() -> Self {
        Self { version: VERSION, groups: Vec::new(), assignments: BTreeMap::new() }
    }
}

impl Groups {
    // ---- persistence -------------------------------------------------------

    /// groups.json v2; a v1 file is converted (kept as groups.v1.json) along with its fold state.
    pub fn load(state_dir: &Path, snapshot: &Snapshot) -> io::Result<Self> {
        let path = state_dir.join(FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let doc: Value = serde_json::from_str(&raw).map_err(io::Error::other)?;
        if doc["version"] == VERSION {
            return serde_json::from_value(doc).map_err(io::Error::other);
        }
        let session = v1_session(&doc)?;
        let groups = Self::from_v1(session, snapshot);
        std::fs::write(state_dir.join(V1_BACKUP), &raw)?;
        Sidebar::from_v1(session).save(state_dir)?;
        groups.save(state_dir)?;
        Ok(groups)
    }

    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        write_atomic(&state_dir.join(FILE), &(json + "\n"))
    }

    /// v1 kept groups and colors by name, assignments by workspace id, and (later) `paths`.
    fn from_v1(session: &Value, snapshot: &Snapshot) -> Self {
        let colors = &session["colors"];
        let mut imported = Self::default();
        for name in session["groups"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            imported = imported.create_with_color(name, colors[name].as_str());
        }
        let by_path = session["paths"].as_object().into_iter().flatten();
        let by_id = session["assignments"].as_object().into_iter().flatten().filter_map(|(id, group)| {
            let ws = snapshot.workspaces.iter().find(|w| &w.workspace_id == id && !w.is_linked_worktree())?;
            Some((PathKey::for_workspace(ws, &snapshot.panes).0, group))
        });
        for (key, group) in by_path.map(|(k, g)| (k.clone(), g)).chain(by_id) {
            if let Some(group) = group.as_str().filter(|g| imported.color_of(g).is_some()) {
                imported = imported.move_to(&key, Some(group));
            }
        }
        imported
    }

    // ---- queries -----------------------------------------------------------

    pub fn color_of(&self, name: &str) -> Option<&str> {
        self.groups.iter().find(|g| g.name == name).map(|g| g.color.as_str())
    }

    pub fn group_of(&self, key: &PathKey) -> Option<&str> {
        self.assignments.get(key.as_str()).map(String::as_str).filter(|g| self.color_of(g).is_some())
    }

    fn next_color(&self) -> &'static str {
        PALETTE
            .iter()
            .copied()
            .find(|c| !self.groups.iter().any(|g| g.color == *c))
            .unwrap_or(PALETTE[self.groups.len() % PALETTE.len()])
    }

    // ---- edits (every op returns a new value) --------------------------------

    pub fn create(&self, name: &str) -> Result<Self, String> {
        let name = name.trim();
        if name.is_empty() || name == UNGROUPED || name.contains(['⟦', '⟧']) {
            return Err(format!("invalid group name {name:?}"));
        }
        if self.color_of(name).is_some() {
            return Err(format!("group {name:?} already exists"));
        }
        Ok(self.create_with_color(name, None))
    }

    fn create_with_color(&self, name: &str, color: Option<&str>) -> Self {
        let color = color.filter(|c| PALETTE.contains(c)).unwrap_or_else(|| self.next_color()).to_owned();
        let mut groups = self.groups.clone();
        groups.push(Group { name: name.to_owned(), color });
        Self { groups, ..self.clone() }
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<Self, String> {
        let new = new.trim();
        if new.is_empty() || new == UNGROUPED || new.contains(['⟦', '⟧']) || self.color_of(new).is_some() {
            return Err(format!("invalid or taken group name {new:?}"));
        }
        if self.color_of(old).is_none() {
            return Err(format!("no group {old:?}"));
        }
        let renamed = |g: &str| if g == old { new.to_owned() } else { g.to_owned() };
        let groups = self.groups.iter().map(|g| Group { name: renamed(&g.name), color: g.color.clone() }).collect();
        let assignments = self.assignments.iter().map(|(k, g)| (k.clone(), renamed(g))).collect();
        Ok(Self { groups, assignments, version: self.version })
    }

    /// Members become Ungrouped (no assignment).
    pub fn delete(&self, name: &str) -> Self {
        Self {
            groups: self.groups.iter().filter(|g| g.name != name).cloned().collect(),
            assignments: self
                .assignments
                .iter()
                .filter(|(_, g)| *g != name)
                .map(|(k, g)| (k.clone(), g.clone()))
                .collect(),
            version: self.version,
        }
    }

    /// `None` = Ungrouped.
    pub fn move_to(&self, path_key: &str, group: Option<&str>) -> Self {
        let mut assignments = self.assignments.clone();
        match group {
            Some(g) => assignments.insert(path_key.to_owned(), g.to_owned()),
            None => assignments.remove(path_key),
        };
        Self { assignments, ..self.clone() }
    }

    /// Move a group by `delta` positions, clamped to the ends.
    pub fn reorder(&self, name: &str, delta: isize) -> Self {
        let Some(from) = self.groups.iter().position(|g| g.name == name) else { return self.clone() };
        let to = (from as isize + delta).clamp(0, self.groups.len() as isize - 1) as usize;
        let mut groups = self.groups.clone();
        let g = groups.remove(from);
        groups.insert(to, g);
        Self { groups, ..self.clone() }
    }

    /// Drop assignments whose real path no longer exists; returns the removed keys.
    pub fn clean(&self, exists: impl Fn(&Path) -> bool) -> (Self, Vec<String>) {
        let (kept, removed): (Vec<_>, Vec<_>) =
            self.assignments.iter().partition(|(k, _)| PathKey(k.to_string()).as_path().is_none_or(&exists));
        let cleaned =
            Self { assignments: kept.into_iter().map(|(k, g)| (k.clone(), g.clone())).collect(), ..self.clone() };
        (cleaned, removed.into_iter().map(|(k, _)| k.clone()).collect())
    }
}

/// The v1 file's session: the only one, else the one with the most assignments.
fn v1_session(doc: &Value) -> io::Result<&Value> {
    let sessions = doc["sessions"].as_object().ok_or_else(|| io::Error::other("groups.json: unknown format"))?;
    sessions
        .values()
        .max_by_key(|s| s["assignments"].as_object().map_or(0, |a| a.len()))
        .ok_or_else(|| io::Error::other("groups.json: v1 file has no sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::types::Worktree;
    use serde_json::json;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("space-kit-groups-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ws(id: &str, label: &str, root: Option<&str>, linked: bool) -> Workspace {
        Workspace {
            workspace_id: id.into(),
            label: label.into(),
            worktree: root.map(|r| Worktree {
                repo_root: r.into(),
                repo_key: format!("{r}/.git"),
                is_linked_worktree: linked,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn path_key_prefers_repo_root_then_pane_cwd_then_label() {
        let tmp = realpath("/tmp");
        assert_eq!(PathKey::for_workspace(&ws("w1", "a", Some("/tmp"), false), &[]).as_str(), tmp);
        assert_eq!(
            PathKey::for_workspace(&ws("w2", "a", Some("/tmp"), true), &[]).as_str(),
            tmp,
            "worktree shares its repo root"
        );
        let panes = [Pane { pane_id: "p".into(), workspace_id: "w3".into(), cwd: Some("/tmp".into()) }];
        assert_eq!(PathKey::for_workspace(&ws("w3", "notes", None, false), &panes).as_str(), tmp);
        assert_eq!(PathKey::for_workspace(&ws("w3", "notes", None, false), &[]).as_str(), "label:notes");
    }

    #[test]
    fn edits_return_new_values() {
        let g = Groups::default().create("Work").unwrap().create("Home").unwrap();
        assert_eq!(g.groups.iter().map(|g| g.color.as_str()).collect::<Vec<_>>(), ["blue", "green"]);
        assert!(g.create("Work").is_err());
        assert!(g.create("⟦x⟧").is_err(), "header brackets are reserved");
        let g = g.move_to("/a", Some("Work")).move_to("/b", Some("Home"));
        let renamed = g.rename("Work", "Job").unwrap();
        assert_eq!(renamed.assignments["/a"], "Job");
        assert_eq!(g.assignments["/a"], "Work", "original untouched");
        assert_eq!(g.reorder("Home", -5).groups[0].name, "Home");
        let deleted = g.delete("Home");
        assert!(!deleted.assignments.contains_key("/b"));
        let (cleaned, removed) = g.move_to("label:x", Some("Work")).clean(|p| p == Path::new("/a"));
        assert_eq!(removed, ["/b"]);
        assert!(cleaned.assignments.contains_key("label:x"));
    }

    #[test]
    fn v1_file_is_imported_once_with_paths_ids_colors_and_folds() {
        let dir = temp_dir("v1");
        let v1 = json!({"version": 1, "sessions": {"k": {
            "groups": ["MADP", "Home"],
            "colors": {"MADP": "peach", "Home": "bogus"},
            "assignments": {"w1": "MADP", "w9": "Home"},
            "paths": {"/repo/closed": "Home"},
            "collapsed": ["Home"],
            "last_focused": "w1",
        }}});
        std::fs::write(dir.join(FILE), v1.to_string()).unwrap();
        let snap = Snapshot { workspaces: vec![ws("w1", "api", Some("/tmp"), false)], ..Default::default() };

        let groups = Groups::load(&dir, &snap).unwrap();
        assert_eq!(groups.color_of("MADP"), Some("peach"));
        assert_eq!(groups.color_of("Home"), Some("blue"), "an invalid color gets the first free one");
        assert_eq!(groups.assignments[&realpath("/tmp")], "MADP");
        assert_eq!(groups.assignments["/repo/closed"], "Home", "closed workspaces keep their group");
        assert!(dir.join(V1_BACKUP).exists());
        assert_eq!(Sidebar::load(&dir).collapsed, ["Home"]);
        assert_eq!(Groups::load(&dir, &Snapshot::default()).unwrap(), groups, "second load reads v2");
        let _ = std::fs::remove_dir_all(dir);
    }
}
