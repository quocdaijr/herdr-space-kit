//! One sync pass: keep header workspaces, order the sidebar, report row and agent tokens, and set
//! the Agents panel title. Every Herdr hook runs this; runs coalesce through a lock.
use super::agents::{self, view_params};
use super::headers::{self, Split};
use super::order::{desired_order, sections};
use super::report::{self, SOURCE};
use super::tokens::{Layout, tokens};
use crate::herdr::client::{Api, Result};
use crate::herdr::types::{AgentStatus, Snapshot};
use crate::model::groups::{Groups, PathKey};
use crate::model::sidebar::Sidebar;
use crate::text::row_width;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{File, TryLockError};
use std::path::Path;

const MAX_PASSES: usize = 3;

pub fn snapshot(api: &dyn Api) -> Result<Snapshot> {
    let mut result = api.call("session.snapshot", json!({}))?;
    serde_json::from_value(result["snapshot"].take()).map_err(|e| format!("session.snapshot: {e}"))
}

/// When a sync is already running, mark it dirty so it runs once more, and return. Interactive
/// callers (the manager, toggle-collapse) pass `wait` to block instead.
pub fn sync(api: &dyn Api, state_dir: &Path, force: bool, wait: bool) -> Result<String> {
    let io = |e: std::io::Error| format!("{}: {e}", state_dir.display());
    std::fs::create_dir_all(state_dir).map_err(io)?;
    let dirty = state_dir.join("sync.dirty");
    let lock = File::create(state_dir.join("sync.lock")).map_err(io)?;
    if wait {
        lock.lock().map_err(io)?;
    } else {
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                File::create(&dirty).map_err(io)?;
                return Ok("another sync is running; it will run again".into());
            }
            Err(TryLockError::Error(e)) => return Err(io(e)),
        }
    }
    let mut summary = String::new();
    for _ in 0..MAX_PASSES {
        let _ = std::fs::remove_file(&dirty);
        summary = sync_once(api, state_dir, force)?;
        if !dirty.exists() {
            break;
        }
    }
    Ok(summary)
}

fn sync_once(api: &dyn Api, state_dir: &Path, force: bool) -> Result<String> {
    let mut snap = snapshot(api)?;
    let groups = Groups::load(state_dir, &snap).map_err(|e| format!("groups.json: {e}"))?;
    let saved = Sidebar::load(state_dir);
    let mut sidebar = saved.normalized(&groups);

    // One header workspace per group that has an open member.
    let split = Split::of(&snap.workspaces);
    let wanted: Vec<&str> = sections(&split.projects, &snap.panes, &groups).iter().filter_map(|s| s.group).collect();
    let (create, close) = split.plan(&wanted);
    if headers::apply(api, state_dir, &create, &close)? {
        snap = snapshot(api)?;
    }
    let split = Split::of(&snap.workspaces);

    // Clicking a header folds/unfolds its group, then returns to the last project.
    let mut focused = snap.focused_workspace_id.clone();
    let clicked = split.headers.iter().find(|(_, w)| Some(&w.workspace_id) == focused.as_ref()).map(|(g, _)| *g);
    let is_project = |id: &str| split.projects.iter().any(|w| w.workspace_id == id);
    if let Some(group) = clicked {
        sidebar = sidebar.toggle(group, &groups);
        if let Some(back) = sidebar.last_focused.clone().filter(|b| is_project(b)) {
            api.call("workspace.focus", json!({"workspace_id": back}))?;
            focused = Some(back);
        }
    } else if let Some(id) = focused.clone().filter(|f| is_project(f)) {
        sidebar.last_focused = Some(id);
    }
    let mut header_ids: Vec<String> = split.headers.iter().map(|(_, w)| w.workspace_id.clone()).collect();
    header_ids.sort();
    sidebar.header_ids = header_ids;

    let sections = sections(&split.projects, &snap.panes, &groups);
    let order = desired_order(&sections, |g| split.header_id(g));
    if !order.iter().eq(snap.workspaces.iter().map(|w| &w.workspace_id)) {
        api.call("workspace.move_block", json!({"workspace_ids": order}))?;
    }

    let mut statuses: BTreeMap<&str, Vec<AgentStatus>> = BTreeMap::new();
    for agent in &snap.agents {
        statuses.entry(agent.workspace_id.as_str()).or_default().push(agent.agent_status);
    }
    let header_map: BTreeMap<&str, &str> = split.headers.iter().map(|(g, w)| (*g, w.workspace_id.as_str())).collect();
    // The branch of every main checkout that gets a `[primary]` row (it has worktrees or agents).
    let branches: BTreeMap<&str, String> = sections
        .iter()
        .flat_map(|s| &s.entries)
        .filter(|e| !e.worktrees.is_empty() || statuses.contains_key(e.ws.workspace_id.as_str()))
        .filter_map(|e| {
            // Herdr leaves `worktree` out for some repos; the first pane's folder still finds it.
            let checkout = e.ws.worktree.as_ref().map(|w| w.checkout_path.as_str()).filter(|p| !p.is_empty()).or_else(|| {
                snap.panes.iter().find(|p| p.workspace_id == e.ws.workspace_id).and_then(|p| p.cwd.as_deref())
            })?;
            Some((e.ws.workspace_id.as_str(), branch_of(checkout)?))
        })
        .collect();
    let width = row_width();
    let layout = Layout { sections: &sections, headers: &header_map, statuses: &statuses, branches: &branches, sidebar: &sidebar, focused: focused.as_deref(), width };
    let workspace_tokens = tokens(&layout);

    // Agents tab: workspaces in sidebar order, each titled "Group › parent › worktree".
    let mut titles: BTreeMap<&str, String> = BTreeMap::new();
    let mut ws_order: Vec<&str> = Vec::new();
    for section in &sections {
        let prefix: Vec<&str> = section.group.into_iter().collect();
        for entry in &section.entries {
            ws_order.push(&entry.ws.workspace_id);
            let branch = branches.get(entry.ws.workspace_id.as_str()).map(String::as_str);
            let own: Vec<&str> = std::iter::once(entry.ws.label.as_str()).chain(branch).collect();
            titles.insert(&entry.ws.workspace_id, [prefix.as_slice(), &own].concat().join(" › "));
            for wt in &entry.worktrees {
                ws_order.push(&wt.workspace_id);
                let path = [prefix.as_slice(), &[entry.ws.label.as_str(), wt.label.as_str()]].concat();
                titles.insert(&wt.workspace_id, path.join(" › "));
            }
        }
    }
    let panes: Vec<&str> = snap.panes.iter().map(|p| p.pane_id.as_str()).collect();
    let agent_tokens = agents::tokens(&ws_order, &snap.agents, &panes, &titles, width);
    let fresh = report::changed(api, state_dir, &workspace_tokens, &agent_tokens, force)?;
    set_agent_view(api, state_dir, &crate::usage::label(), fresh);

    if sidebar != saved {
        sidebar.save(state_dir).map_err(|e| format!("sidebar.json: {e}"))?;
    }
    let grouped = sections.iter().filter(|s| s.group.is_some()).map(|s| s.entries.len()).sum::<usize>();
    Ok(format!("Synced {grouped} grouped workspace(s) across {} group(s).", groups.groups.len()))
}

/// The branch a checkout is on, from its `HEAD` (a short commit id when detached). Looks up from
/// `dir` for `.git`, which is a directory, or a `gitdir:` file in linked worktrees and submodules.
pub fn branch_of(dir: &str) -> Option<String> {
    let dot_git = Path::new(dir).ancestors().map(|d| d.join(".git")).find(|g| g.exists())?;
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(file) => {
            let target = Path::new(file.trim().strip_prefix("gitdir:")?.trim());
            dot_git.parent()?.join(target)
        }
        Err(_) => dot_git,
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_owned()),
        None => Some(head.get(..7)?.to_owned()),
    }
}

/// Apply the agent view when the usage label changed, or after a full resend (agent views do not
/// survive a server restart either).
fn set_agent_view(api: &dyn Api, state_dir: &Path, label: &str, fresh: bool) {
    let path = state_dir.join("view-label.txt");
    if !fresh && std::fs::read_to_string(&path).ok().as_deref() == Some(label) {
        return;
    }
    let applied = api
        .call("agent.view.set", view_params(SOURCE, label))
        .and_then(|_| std::fs::write(&path, label).map_err(|e| e.to_string()));
    if let Err(e) = applied {
        eprintln!("space-kit: agent view: {e}");
    }
}

/// `workspace.focused` only matters when a group is folded or a header was clicked.
pub fn focus_needs_sync(state_dir: &Path) -> bool {
    let event = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    let sidebar = Sidebar::load(state_dir);
    event.contains('⟦') || sidebar.header_ids.iter().any(|id| event.contains(&format!("\"{id}\""))) || !sidebar.collapsed.is_empty()
}

/// Fold or unfold the group of `workspace` (a worktree folds its repo's group).
pub fn toggle_collapse(api: &dyn Api, state_dir: &Path, workspace: Option<&str>) -> Result<String> {
    let id = workspace.ok_or("no workspace to toggle (pass --workspace)")?;
    let snap = snapshot(api)?;
    let groups = Groups::load(state_dir, &snap).map_err(|e| format!("groups.json: {e}"))?;
    let ws = snap.workspaces.iter().find(|w| w.workspace_id == id).ok_or_else(|| format!("no workspace {id}"))?;
    let group = match headers::group_of(ws) {
        Some(header) => header.to_owned(),
        None => groups.group_of(&PathKey::for_workspace(ws, &snap.panes)).ok_or("this workspace is not in a group")?.to_owned(),
    };
    let sidebar = Sidebar::load(state_dir).toggle(&group, &groups);
    sidebar.save(state_dir).map_err(|e| format!("sidebar.json: {e}"))?;
    sync(api, state_dir, false, true)?;
    Ok(format!("{group}: {}", if sidebar.is_folded(&group) { "collapsed" } else { "expanded" }))
}

/// The workspace an action was invoked from.
pub fn context_workspace_id() -> Option<String> {
    let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok().and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    context
        .and_then(|c| c["workspace_id"].as_str().filter(|id| !id.is_empty()).map(str::to_owned))
        .or_else(|| std::env::var("HERDR_WORKSPACE_ID").ok().filter(|id| !id.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::report::tests::{Fake, temp_dir};
    use serde_json::Value;

    fn snap(focused: &str) -> Value {
        json!({
            "workspaces": [
                {"workspace_id": "p1", "label": "one", "worktree": {"repo_root": "/tmp/space-kit-one", "repo_key": "/tmp/space-kit-one/.git"}},
                {"workspace_id": "p2", "label": "two", "worktree": {"repo_root": "/tmp/space-kit-two", "repo_key": "/tmp/space-kit-two/.git"}},
                {"workspace_id": "h1", "label": "⟦A⟧"},
            ],
            "panes": [], "agents": [], "focused_workspace_id": focused,
        })
    }

    fn setup(name: &str) -> std::path::PathBuf {
        let dir = temp_dir(name);
        let two = PathKey::for_workspace(&serde_json::from_value(snap("p1")["workspaces"][1].clone()).unwrap(), &[]);
        Groups::default().create("A").unwrap().move_to(two.as_str(), Some("A")).save(&dir).unwrap();
        Sidebar { last_focused: Some("p1".into()), ..Default::default() }.save(&dir).unwrap();
        dir
    }

    #[test]
    fn orders_workspaces_under_their_header_and_folds_on_header_click() {
        let dir = setup("sync-click");
        let api = Fake { snapshot: snap("h1"), ..Default::default() };
        sync(&api, &dir, true, true).unwrap();
        let sent = api.sent.borrow();
        let moved = sent.iter().find(|(m, _)| m == "workspace.move_block").unwrap();
        assert_eq!(moved.1["workspace_ids"], json!(["h1", "p2", "p1"]));
        assert!(sent.iter().any(|(m, p)| m == "workspace.focus" && p["workspace_id"] == "p1"));
        assert!(sent.iter().any(|(m, _)| m == "agent.view.set"));
        drop(sent);
        assert_eq!(Sidebar::load(&dir).collapsed, ["A"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_header_is_created_and_unwanted_ones_closed() {
        let dir = setup("sync-create");
        let mut s = snap("p1");
        s["workspaces"][2]["label"] = json!("⟦Gone⟧");
        let api = Fake { snapshot: s, ..Default::default() };
        sync(&api, &dir, true, true).unwrap();
        let sent = api.sent.borrow();
        assert!(sent.iter().any(|(m, p)| m == "workspace.close" && p["workspace_id"] == "h1"));
        let created = sent.iter().find(|(m, _)| m == "workspace.create").unwrap();
        assert_eq!(created.1["label"], "⟦A⟧");
        assert_eq!(created.1["cwd"], dir.join("headers/A").to_string_lossy().as_ref());
        drop(sent);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn branch_comes_from_git_head() {
        let dir = temp_dir("branch");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(branch_of(dir.to_str().unwrap()).as_deref(), Some("feat/x"));
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        assert_eq!(branch_of(dir.join("src/deep").to_str().unwrap()).as_deref(), Some("feat/x"), "found from a subfolder");
        std::fs::write(dir.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch_of(dir.to_str().unwrap()).as_deref(), Some("0123456"));
        let linked = dir.join("wt");
        std::fs::create_dir_all(linked.join("../gitdirs/wt")).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: ../gitdirs/wt\n").unwrap();
        std::fs::write(dir.join("gitdirs/wt/HEAD"), "ref: refs/heads/fix-1\n").unwrap();
        assert_eq!(branch_of(linked.to_str().unwrap()).as_deref(), Some("fix-1"), "a gitdir file is followed");
        assert_eq!(branch_of("/nonexistent"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn second_sync_reports_nothing_new() {
        let dir = setup("sync-cache");
        let api = Fake { snapshot: snap("p2"), ..Default::default() };
        sync(&api, &dir, true, true).unwrap();
        api.sent.borrow_mut().clear();
        sync(&api, &dir, false, true).unwrap();
        let methods = api.methods();
        assert!(!methods.iter().any(|m| m.ends_with("report_metadata")), "{methods:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
