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
/// Foreground process names that are an idle shell, safe to type the overview command into.
const SHELLS: [&str; 5] = ["zsh", "bash", "fish", "sh", "dash"];
/// Pids of the header shells already typed into, one per line.
const OVERVIEW_SENT: &str = "overview-sent.txt";

pub fn snapshot(api: &dyn Api) -> Result<Snapshot> {
    let mut result = api.call("session.snapshot", json!({}))?;
    serde_json::from_value(result["snapshot"].take()).map_err(|e| format!("session.snapshot: {e}"))
}

/// When a sync is already running, mark it dirty so it runs once more, and return; a forced
/// request writes `force` into the marker so the rerun resends everything (the startup hook's
/// `--force` must survive racing an event hook). Interactive callers (the manager,
/// toggle-collapse) pass `wait` to block instead.
pub fn sync(api: &dyn Api, state_dir: &Path, force: bool, wait: bool) -> Result<String> {
    run(api, state_dir, force, wait, None)
}

/// A change to sidebar.json made under the sync lock, so a running sync cannot save over it.
#[derive(Clone, Copy)]
enum Change<'a> {
    Toggle(&'a str),
    /// A header click: unfold the group when `unfold_on_click` is on.
    Unfold(&'a str),
    /// Flip `unfold_on_click`.
    UnfoldOnClick,
}

/// Fold or unfold `group`, then sync. The flip happens under the sync lock, so a running sync
/// cannot save over it.
pub fn toggle_group(api: &dyn Api, state_dir: &Path, group: &str) -> Result<String> {
    run(api, state_dir, false, true, Some(Change::Toggle(group)))
}

/// Flip the `unfold_on_click` setting, then sync.
pub fn toggle_unfold_on_click(api: &dyn Api, state_dir: &Path) -> Result<String> {
    run(api, state_dir, false, true, Some(Change::UnfoldOnClick))
}

/// A click on a header (a `workspace.focused` event for it) opens its overview, and unfolds the
/// group when `unfold_on_click` is on; otherwise only the overview's `Space` folds. Focus stays on
/// the header, so the sidebar does not scroll.
pub fn header_click(api: &dyn Api, state_dir: &Path, header_id: &str) -> Result<String> {
    let snap = snapshot(api)?;
    let ws = snap
        .workspaces
        .iter()
        .find(|w| w.workspace_id == header_id)
        .ok_or_else(|| format!("no workspace {header_id}"))?;
    let group = headers::group_of(ws).ok_or_else(|| format!("{header_id} is not a group header"))?;
    run(api, state_dir, false, true, Some(Change::Unfold(group)))
}

fn run(api: &dyn Api, state_dir: &Path, force: bool, wait: bool, change: Option<Change>) -> Result<String> {
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
    if let Some(change) = change {
        apply(api, state_dir, change)?;
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

fn apply(api: &dyn Api, state_dir: &Path, change: Change) -> Result<()> {
    let sidebar = Sidebar::load(state_dir);
    let group = match change {
        Change::UnfoldOnClick => None,
        Change::Toggle(group) => Some(group),
        Change::Unfold(group) => (sidebar.is_folded(group) && sidebar.unfold_on_click).then_some(group),
    };
    let next = match (change, group) {
        (Change::UnfoldOnClick, _) => Sidebar { unfold_on_click: !sidebar.unfold_on_click, ..sidebar },
        (_, Some(group)) => {
            let snap = snapshot(api)?;
            let groups = Groups::load(state_dir, &snap).map_err(|e| format!("groups.json: {e}"))?;
            sidebar.toggle(group, &groups)
        }
        (_, None) => return Ok(()),
    };
    next.save(state_dir).map_err(|e| format!("sidebar.json: {e}"))
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

    let focused = snap.focused_workspace_id.clone();
    let mut header_ids: Vec<String> = split.headers.iter().map(|(_, w)| w.workspace_id.clone()).collect();
    header_ids.sort();
    sidebar.header_ids = header_ids;
    if let Ok(exe) = std::env::current_exe() {
        ensure_overviews(api, state_dir, &exe.to_string_lossy(), &split, &snap);
    }

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

/// `workspace.focused` only needs a sync when a group is folded: folded groups keep the focused
/// row showing.
pub fn focus_needs_sync(state_dir: &Path) -> bool {
    !Sidebar::load(state_dir).collapsed.is_empty()
}

/// The header a `workspace.focused` event landed on, if it did (a click on that header).
pub fn focused_header(state_dir: &Path) -> Option<String> {
    let event = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok()?;
    let event: serde_json::Value = serde_json::from_str(&event).ok()?;
    let id = event["data"]["workspace_id"].as_str()?;
    Sidebar::load(state_dir).header_ids.iter().find(|h| *h == id).cloned()
}

/// Keep the group overview running in every header's pane. Types only into a pane whose
/// foreground is an idle shell, so it also comes back after a restart (Herdr restores shells,
/// not their programs). Each shell (by `shell_pid`, which `exec` keeps) is typed into once: until it
/// reads the line, every sync still sees it idle, and the extra copies would reach the overview as keys.
/// Best effort: an error is reported, not fatal.
fn ensure_overviews(api: &dyn Api, state_dir: &Path, exe: &str, split: &Split, snap: &Snapshot) {
    let path = state_dir.join(OVERVIEW_SENT);
    let sent: Vec<u64> =
        std::fs::read_to_string(&path).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect();
    // Only shells seen this pass are kept, so pids of gone shells drop out.
    let mut keep: Vec<u64> = Vec::new();
    let mut complete = true;
    'headers: for (_, header) in &split.headers {
        let id = header.workspace_id.as_str();
        let mut shell = None;
        for pane in snap.panes.iter().filter(|p| p.workspace_id == id) {
            let info = match api.call("pane.process_info", json!({"pane_id": pane.pane_id})) {
                Ok(info) => info,
                Err(e) => {
                    eprintln!("space-kit: overview: {e}");
                    complete = false;
                    break 'headers;
                }
            };
            let procs = info["process_info"]["foreground_processes"].as_array().cloned().unwrap_or_default();
            let runs_overview = procs.iter().any(|p| {
                p["name"] == "space-kit" && p["argv"].as_array().is_some_and(|a| a.iter().any(|x| x == "overview"))
            });
            if runs_overview {
                shell = None;
                break;
            }
            let is_shell = procs.iter().any(|p| p["name"].as_str().is_some_and(|n| SHELLS.contains(&n)));
            if is_shell && shell.is_none() {
                // The pane's shell, not the foreground pid: a starting shell's foreground is a new
                // subshell each time.
                shell = Some((pane.pane_id.as_str(), info["process_info"]["shell_pid"].as_u64()));
            }
        }
        let Some((pane, pid)) = shell else { continue };
        if let Some(pid) = pid.filter(|p| sent.contains(p)) {
            keep.push(pid);
            continue;
        }
        let text = format!("exec {} overview {id}\r", shell_quote(exe));
        match api.call("pane.send_text", json!({"pane_id": pane, "text": text})) {
            // Recorded only once sent, so a failed send is retried on the next sync.
            Ok(_) => keep.extend(pid),
            Err(e) => eprintln!("space-kit: overview: {e}"),
        }
    }
    if !complete {
        // Headers after the error went unchecked: keep their shells' pids too.
        let unchecked: Vec<u64> = sent.iter().copied().filter(|p| !keep.contains(p)).collect();
        keep.extend(unchecked);
    }
    let lines: String = keep.iter().map(|pid| format!("{pid}\n")).collect();
    if let Err(e) = crate::model::write_atomic(&path, &lines) {
        eprintln!("space-kit: overview: {}: {e}", path.display());
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
    toggle_group(api, state_dir, &group)?;
    let folded = Sidebar::load(state_dir).is_folded(&group);
    Ok(format!("{group}: {}", if folded { "collapsed" } else { "expanded" }))
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
        dir
    }

    #[test]
    fn orders_workspaces_under_their_header_and_a_focused_header_is_no_click() {
        let dir = setup("sync-click");
        let api = Fake { snapshot: snap("h1"), ..Default::default() };
        sync(&api, &dir, true, true).unwrap();
        let sent = api.sent.borrow();
        let moved = sent.iter().find(|(m, _)| m == "workspace.move_block").unwrap();
        assert_eq!(moved.1["workspace_ids"], json!(["h1", "p2", "p1"]));
        assert!(!sent.iter().any(|(m, _)| m == "workspace.focus"), "focus stays on the header");
        assert!(sent.iter().any(|(m, _)| m == "agent.view.set"));
        drop(sent);
        assert!(Sidebar::load(&dir).collapsed.is_empty(), "only a click event toggles");
        assert_eq!(Sidebar::load(&dir).header_ids, ["h1"]);
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
    fn a_focus_event_names_the_clicked_header_and_syncs_only_when_folded() {
        let dir = setup("sync-focus");
        Sidebar { header_ids: vec!["h1".into()], ..Default::default() }.save(&dir).unwrap();
        let event = |id: &str| format!(r#"{{"event":"workspace.focused","data":{{"workspace_id":"{id}"}}}}"#);
        // SAFETY: tests touching this variable run in this one test only.
        unsafe { std::env::set_var("HERDR_PLUGIN_EVENT_JSON", event("h1")) };
        assert_eq!(focused_header(&dir).as_deref(), Some("h1"));
        unsafe { std::env::set_var("HERDR_PLUGIN_EVENT_JSON", event("p2")) };
        assert_eq!(focused_header(&dir), None);
        unsafe { std::env::remove_var("HERDR_PLUGIN_EVENT_JSON") };
        assert_eq!(focused_header(&dir), None);
        assert!(!focus_needs_sync(&dir), "nothing folded: no sync");
        Sidebar { collapsed: vec!["A".into()], ..Default::default() }.save(&dir).unwrap();
        assert!(focus_needs_sync(&dir));
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
    fn a_header_click_only_unfolds_and_keeps_focus_there() {
        let dir = setup("sync-header-click");
        let api = Fake { snapshot: snap("h1"), ..Default::default() };
        header_click(&api, &dir, "h1").unwrap();
        assert!(Sidebar::load(&dir).collapsed.is_empty(), "an unfolded group stays unfolded");
        toggle_group(&api, &dir, "A").unwrap();
        header_click(&api, &dir, "h1").unwrap();
        assert_eq!(Sidebar::load(&dir).collapsed, ["A"], "by default a click leaves the fold alone");
        toggle_unfold_on_click(&api, &dir).unwrap();
        assert!(Sidebar::load(&dir).unfold_on_click);
        header_click(&api, &dir, "h1").unwrap();
        assert!(Sidebar::load(&dir).collapsed.is_empty(), "with the setting on, a click unfolds");
        header_click(&api, &dir, "h1").unwrap();
        assert!(Sidebar::load(&dir).collapsed.is_empty(), "and never folds");
        assert!(!api.methods().contains(&"workspace.focus".to_owned()));
        assert_eq!(header_click(&api, &dir, "p1"), Err("p1 is not a group header".into()));
        assert_eq!(header_click(&api, &dir, "nope"), Err("no workspace nope".into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Answers `pane.process_info` with one foreground process per pane, every shell with pid `.2`.
    struct Procs(
        BTreeMap<&'static str, (&'static str, &'static [&'static str])>,
        std::cell::RefCell<Vec<Value>>,
        std::cell::Cell<u32>,
        std::cell::Cell<u32>,
        std::cell::Cell<bool>,
    );

    impl Api for Procs {
        fn call(&self, method: &str, params: Value) -> Result<Value> {
            if method == "pane.send_text" {
                if self.4.get() {
                    return Err("pane.send_text: down".into());
                }
                self.1.borrow_mut().push(params);
                return Ok(json!({}));
            }
            let pane = params["pane_id"].as_str().unwrap_or_default();
            let Some((name, argv)) = self.0.get(pane) else { return Err(format!("{method}: no pane {pane}")) };
            // A starting shell's foreground is a fresh subshell each time; `shell_pid` stays put.
            let fresh = self.3.get() + 1;
            self.3.set(fresh);
            Ok(json!({"process_info": {
                "shell_pid": self.2.get(),
                "foreground_processes": [{"pid": 1000 + fresh, "name": name, "argv": argv}],
            }}))
        }
    }

    #[test]
    fn overviews_start_in_an_idle_header_shell_only() {
        let snap: Snapshot = serde_json::from_value(json!({
            "workspaces": [{"workspace_id": "h1", "label": "⟦A⟧"}, {"workspace_id": "h2", "label": "⟦B⟧"}, {"workspace_id": "h3", "label": "⟦C⟧"}],
            "panes": [
                {"pane_id": "h1:p2", "workspace_id": "h1"}, {"pane_id": "h1:p1", "workspace_id": "h1"},
                {"pane_id": "h2:p1", "workspace_id": "h2"}, {"pane_id": "h2:p2", "workspace_id": "h2"},
                {"pane_id": "h3:p1", "workspace_id": "h3"},
            ],
        }))
        .unwrap();
        let split = Split::of(&snap.workspaces);
        let procs = BTreeMap::from([
            ("h1:p2", ("herdr-sidebar", &[][..])),
            ("h1:p1", ("zsh", &["-zsh"][..])),
            ("h2:p1", ("zsh", &["-zsh"][..])),
            ("h2:p2", ("space-kit", &["/x/space-kit", "overview", "h2"][..])),
            ("h3:p1", ("vim", &["vim"][..])),
        ]);
        let dir = temp_dir("overview-sent");
        let api = Procs(procs, Default::default(), std::cell::Cell::new(1), Default::default(), Default::default());
        ensure_overviews(&api, &dir, "/it's/space-kit", &split, &snap);
        let sent = api.1.borrow();
        assert_eq!(sent.len(), 1, "B already runs it, C is busy: {sent:?}");
        assert_eq!(sent[0]["pane_id"], "h1:p1");
        assert_eq!(sent[0]["text"], "exec '/it'\\''s/space-kit' overview h1\r");
        drop(sent);
        ensure_overviews(&api, &dir, "/it's/space-kit", &split, &snap);
        assert_eq!(api.1.borrow().len(), 1, "a shell that has not run it yet is not typed into twice");
        api.2.set(2); // a new shell, as after a server restart
        ensure_overviews(&api, &dir, "/it's/space-kit", &split, &snap);
        assert_eq!(api.1.borrow().len(), 2, "a new shell gets it once");
        let broken =
            Procs(BTreeMap::new(), Default::default(), Default::default(), Default::default(), Default::default());
        ensure_overviews(&broken, &dir, "/x", &split, &snap); // reported, not fatal
        assert!(broken.1.borrow().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overview_sends_are_recorded_only_when_sent_and_kept_across_errors() {
        // h2's pane is unknown to the fake, so its process_info fails after h1 was handled.
        let snap: Snapshot = serde_json::from_value(json!({
            "workspaces": [{"workspace_id": "h1", "label": "⟦A⟧"}, {"workspace_id": "h2", "label": "⟦B⟧"}],
            "panes": [{"pane_id": "h1:p1", "workspace_id": "h1"}, {"pane_id": "h2:p1", "workspace_id": "h2"}],
        }))
        .unwrap();
        let split = Split::of(&snap.workspaces);
        let procs = BTreeMap::from([("h1:p1", ("zsh", &["-zsh"][..]))]);
        let dir = temp_dir("overview-retry");
        let api =
            Procs(procs, Default::default(), std::cell::Cell::new(7), Default::default(), std::cell::Cell::new(true));
        ensure_overviews(&api, &dir, "/x", &split, &snap);
        assert!(api.1.borrow().is_empty(), "the send failed");
        api.4.set(false);
        ensure_overviews(&api, &dir, "/x", &split, &snap);
        assert_eq!(api.1.borrow().len(), 1, "a failed send is retried");
        ensure_overviews(&api, &dir, "/x", &split, &snap);
        assert_eq!(api.1.borrow().len(), 1, "a send before a later error is still recorded");
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
