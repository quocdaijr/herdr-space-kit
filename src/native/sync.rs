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
const FORCE: &str = "force";
/// Focus landing on a header again this soon after it toggled is an echo, not a click (the
/// herdr-sidebar plugin refocuses its pane in the workspace that was focused at the click).
/// ponytail: a time window, so a real fold-then-unfold inside it is missed; use an event source
/// field instead if Herdr adds one.
const CLICK_ECHO_MS: u64 = 1500;

fn now_ms() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

pub fn snapshot(api: &dyn Api) -> Result<Snapshot> {
    let mut result = api.call("session.snapshot", json!({}))?;
    serde_json::from_value(result["snapshot"].take()).map_err(|e| format!("session.snapshot: {e}"))
}

/// When a sync is already running, mark it dirty so it runs once more, and return; a forced
/// request writes `force` into the marker so the rerun resends everything (the startup hook's
/// `--force` must survive racing an event hook). Interactive callers (the manager,
/// toggle-collapse) pass `wait` to block instead.
pub fn sync(api: &dyn Api, state_dir: &Path, force: bool, wait: bool) -> Result<String> {
    let io = |e: std::io::Error| format!("{}: {e}", state_dir.display());
    std::fs::create_dir_all(state_dir).map_err(io)?;
    let dirty = state_dir.join("sync.dirty");
    let lock = File::create(state_dir.join("sync.lock")).map_err(io)?;
    let acquired = if wait { lock.lock().map(|()| true) } else { got_lock(lock.try_lock()) };
    if !acquired.map_err(io)? {
        // Append, so a pending `force` is never overwritten by a plain request.
        let mut marker = std::fs::OpenOptions::new().create(true).append(true).open(&dirty).map_err(io)?;
        if force {
            std::io::Write::write_all(&mut marker, FORCE.as_bytes()).map_err(io)?;
        }
        return Ok("another sync is running; it will run again".into());
    }
    // A marker left by an earlier run that hit MAX_PASSES still counts.
    let mut force = force | take_dirty(&dirty).unwrap_or(false);
    let mut summary = String::new();
    for pass in 1..=MAX_PASSES {
        summary = sync_once(api, state_dir, force)?;
        if pass == MAX_PASSES {
            break; // leave any new marker for the next sync
        }
        match take_dirty(&dirty) {
            Some(forced) => force = forced,
            None => break,
        }
    }
    Ok(summary)
}

/// Whether `try_lock` got the lock (false: another sync holds it).
fn got_lock(result: std::result::Result<(), TryLockError>) -> std::io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Read and remove the dirty marker: None when absent, else whether a run asked for `force`.
fn take_dirty(path: &Path) -> Option<bool> {
    let marker = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    Some(marker.contains(FORCE))
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

    // Clicking a header folds/unfolds its group, then returns to the last project (or the group's
    // first one), so focus never rests on a header: the next click always fires an event.
    let mut focused = snap.focused_workspace_id.clone();
    let clicked = split.headers.iter().find(|(_, w)| Some(&w.workspace_id) == focused.as_ref()).copied();
    let is_project = |id: &str| split.projects.iter().any(|w| w.workspace_id == id);
    if let Some((group, header)) = clicked {
        let now = now_ms();
        let echo = sidebar
            .last_toggle
            .as_ref()
            .is_some_and(|(id, at)| *id == header.workspace_id && now.saturating_sub(*at) < CLICK_ECHO_MS);
        if !echo {
            sidebar = sidebar.toggle(group, &groups);
            sidebar.last_toggle = Some((header.workspace_id.clone(), now));
        }
        let first_member = || {
            let section = sections(&split.projects, &snap.panes, &groups).into_iter().find(|s| s.group == Some(group));
            section.and_then(|s| s.entries.first().map(|e| e.ws.workspace_id.clone()))
        };
        if let Some(back) = sidebar.last_focused.clone().filter(|b| is_project(b)).or_else(first_member) {
            api.call("workspace.focus", json!({"workspace_id": back}))?;
            sidebar.last_focused = Some(back.clone());
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
            let checkout =
                e.ws.worktree.as_ref().map(|w| w.checkout_path.as_str()).filter(|p| !p.is_empty()).or_else(|| {
                    snap.panes.iter().find(|p| p.workspace_id == e.ws.workspace_id).and_then(|p| p.cwd.as_deref())
                })?;
            Some((e.ws.workspace_id.as_str(), branch_of(checkout)?))
        })
        .collect();
    let width = row_width();
    let layout = Layout {
        sections: &sections,
        headers: &header_map,
        statuses: &statuses,
        branches: &branches,
        sidebar: &sidebar,
        focused: focused.as_deref(),
        width,
    };
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

/// `workspace.focused` only needs a sync when a group is folded or a header was clicked. A
/// skipped event still records the workspace as `last_focused`, so a later header click returns
/// to the right place.
pub fn focus_needs_sync(state_dir: &Path) -> bool {
    let event = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    let sidebar = Sidebar::load(state_dir);
    if event.contains('⟦')
        || sidebar.header_ids.iter().any(|id| event.contains(&format!("\"{id}\"")))
        || !sidebar.collapsed.is_empty()
    {
        return true;
    }
    let focused = serde_json::from_str::<serde_json::Value>(&event)
        .ok()
        .and_then(|e| e["data"]["workspace_id"].as_str().map(str::to_owned));
    if focused.is_some() && focused != sidebar.last_focused {
        // Best effort: a failed write only means the next header click goes back less precisely.
        let _ = Sidebar { last_focused: focused, ..sidebar }.save(state_dir);
    }
    false
}

/// Fold or unfold the group of `workspace` (a worktree folds its repo's group).
pub fn toggle_collapse(api: &dyn Api, state_dir: &Path, workspace: Option<&str>) -> Result<String> {
    let id = workspace.ok_or("no workspace to toggle (pass --workspace)")?;
    let snap = snapshot(api)?;
    let groups = Groups::load(state_dir, &snap).map_err(|e| format!("groups.json: {e}"))?;
    let ws = snap.workspaces.iter().find(|w| w.workspace_id == id).ok_or_else(|| format!("no workspace {id}"))?;
    let group = match headers::group_of(ws) {
        Some(header) => header.to_owned(),
        None => groups
            .group_of(&PathKey::for_workspace(ws, &snap.panes))
            .ok_or("this workspace is not in a group")?
            .to_owned(),
    };
    let sidebar = Sidebar::load(state_dir).toggle(&group, &groups);
    sidebar.save(state_dir).map_err(|e| format!("sidebar.json: {e}"))?;
    sync(api, state_dir, false, true)?;
    Ok(format!("{group}: {}", if sidebar.is_folded(&group) { "collapsed" } else { "expanded" }))
}

/// The workspace an action was invoked from.
pub fn context_workspace_id() -> Option<String> {
    let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
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
        assert_eq!(
            branch_of(dir.join("src/deep").to_str().unwrap()).as_deref(),
            Some("feat/x"),
            "found from a subfolder"
        );
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
    fn a_forced_request_that_hits_the_lock_forces_the_rerun() {
        let dir = setup("sync-force");
        let lock = File::create(dir.join("sync.lock")).unwrap();
        lock.lock().unwrap();
        let api = Fake { snapshot: snap("p2"), ..Default::default() };
        sync(&api, &dir, true, false).unwrap();
        sync(&api, &dir, false, false).unwrap();
        assert!(api.sent.borrow().is_empty(), "both calls only left a marker");
        assert_eq!(take_dirty(&dir.join("sync.dirty")), Some(true), "the plain call kept the force");
        drop(lock);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_skipped_focus_event_still_records_the_last_focused_workspace() {
        let dir = setup("sync-focus");
        // SAFETY: tests touching this variable run in this one test only.
        unsafe {
            std::env::set_var(
                "HERDR_PLUGIN_EVENT_JSON",
                r#"{"event":"workspace.focused","data":{"workspace_id":"p2"}}"#,
            )
        };
        assert!(!focus_needs_sync(&dir), "nothing folded: no sync");
        assert_eq!(Sidebar::load(&dir).last_focused.as_deref(), Some("p2"));
        unsafe { std::env::remove_var("HERDR_PLUGIN_EVENT_JSON") };
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn toggle_collapse_folds_the_group_of_a_member_or_its_header() {
        let dir = setup("sync-toggle");
        let api = Fake { snapshot: snap("p2"), ..Default::default() };
        assert_eq!(toggle_collapse(&api, &dir, Some("p2")).unwrap(), "A: collapsed");
        assert_eq!(toggle_collapse(&api, &dir, Some("h1")).unwrap(), "A: expanded", "a header toggles its group");
        assert_eq!(toggle_collapse(&api, &dir, Some("p1")), Err("this workspace is not in a group".into()));
        assert_eq!(toggle_collapse(&api, &dir, Some("nope")), Err("no workspace nope".into()));
        assert!(toggle_collapse(&api, &dir, None).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_lock_result_says_whether_this_sync_runs() {
        assert!(got_lock(Ok(())).unwrap());
        assert!(!got_lock(Err(TryLockError::WouldBlock)).unwrap());
        assert!(got_lock(Err(TryLockError::Error(std::io::ErrorKind::Other.into()))).is_err());
    }

    #[test]
    fn requests_arriving_mid_sync_rerun_it_up_to_the_limit() {
        let dir = setup("sync-reruns");
        let marker = dir.join("sync.dirty");
        let on_call = move |method: &str| {
            if method == "session.snapshot" {
                std::fs::write(&marker, "").unwrap(); // another hook asked for a sync meanwhile
            }
        };
        let api = Fake { snapshot: snap("p2"), on_call: Some(Box::new(on_call)), ..Default::default() };
        sync(&api, &dir, false, true).unwrap();
        assert_eq!(api.methods().iter().filter(|m| *m == "session.snapshot").count(), MAX_PASSES);
        assert!(dir.join("sync.dirty").exists(), "the last request is left for the next sync");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_header_click_without_a_known_last_project_goes_to_the_group() {
        let dir = setup("sync-click-unknown");
        Sidebar { last_focused: Some("closed".into()), ..Default::default() }.save(&dir).unwrap();
        let api = Fake { snapshot: snap("h1"), ..Default::default() };
        sync(&api, &dir, true, true).unwrap();
        assert!(api.sent.borrow().iter().any(|(m, p)| m == "workspace.focus" && p["workspace_id"] == "p2"));
        assert_eq!(Sidebar::load(&dir).collapsed, ["A"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_header_refocused_right_after_a_click_is_not_a_second_click() {
        let dir = setup("sync-click-echo");
        let api = Fake { snapshot: snap("h1"), ..Default::default() };
        sync(&api, &dir, true, true).unwrap();
        sync(&api, &dir, false, true).unwrap();
        assert_eq!(Sidebar::load(&dir).collapsed, ["A"], "the echo did not unfold it");
        let focus_calls = api.methods().iter().filter(|m| *m == "workspace.focus").count();
        assert_eq!(focus_calls, 2, "both times focus leaves the header");

        let old = Sidebar { last_toggle: Some(("h1".into(), 0)), ..Sidebar::load(&dir) };
        old.save(&dir).unwrap();
        sync(&api, &dir, false, true).unwrap();
        assert!(Sidebar::load(&dir).collapsed.is_empty(), "a later click toggles again");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn branch_rows_titles_and_view_errors() {
        let dir = setup("sync-branches");
        let repo = dir.join("plain-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        let snapshot = json!({
            "workspaces": [
                {"workspace_id": "p2", "label": "two", "worktree": {"repo_root": "/tmp/space-kit-two", "repo_key": "/tmp/space-kit-two/.git", "checkout_path": ""}},
                {"workspace_id": "wt", "label": "fix", "worktree": {"repo_root": "/tmp/space-kit-two", "repo_key": "/tmp/space-kit-two/.git", "is_linked_worktree": true}},
                {"workspace_id": "pl", "label": "plain"},
            ],
            "panes": [{"pane_id": "pl:1", "workspace_id": "pl", "cwd": repo}, {"pane_id": "wt:1", "workspace_id": "wt"}],
            "agents": [
                {"pane_id": "pl:1", "workspace_id": "pl", "agent": "claude", "agent_status": "idle"},
                {"pane_id": "wt:1", "workspace_id": "wt", "agent": "codex", "agent_status": "working"},
            ],
        });
        let api = Fake { snapshot, fail: &["agent.view.set"], ..Default::default() };
        sync(&api, &dir, true, true).unwrap(); // a failed agent view is reported, not fatal
        let tokens = |id: &str| {
            api.sent
                .borrow()
                .iter()
                .filter(|(_, p)| p["workspace_id"] == id || p["pane_id"] == id)
                .map(|(_, p)| p["tokens"].clone())
                .collect::<Vec<_>>()
        };
        let branch_found = |t: &Value| t["group_branch"].as_str().is_some_and(|b| b.starts_with("⠀⠀⠀[primary] - f"));
        assert!(tokens("pl").iter().any(branch_found), "branch found from the pane folder: {:?}", tokens("pl"));
        assert!(
            tokens("wt:1").iter().any(|t| t["agent_header"].as_str().is_some_and(|h| h.starts_with("━ A › two › fix")))
        );
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
