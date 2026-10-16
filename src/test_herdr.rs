//! A fake Herdr socket for unit tests. In test builds `socket_path()` always points here, so no
//! test can reach a live Herdr server.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::OnceLock;

/// The snapshot `session.snapshot` returns: the same fixture the manager tests use.
pub fn snapshot() -> Value {
    json!({
        "focused_workspace_id": "a",
        "workspaces": [
            {"workspace_id": "a", "label": "api", "worktree": {"repo_root": "/space-kit-fake/api", "repo_key": "/space-kit-fake/api/.git"}},
            {"workspace_id": "n", "label": "notes"},
        ],
        "agents": [{"pane_id": "a:p1", "workspace_id": "a", "agent": "claude", "agent_status": "idle"}],
        "panes": [{"pane_id": "a:p1", "workspace_id": "a"}],
    })
}

fn answer(request: &Value) -> Vec<Value> {
    let id = &request["id"];
    let ok = |result: Value| json!({"id": id, "result": result});
    match request["method"].as_str().unwrap_or("") {
        "session.snapshot" => vec![ok(json!({"snapshot": snapshot()}))],
        // Ack, one event, then the connection closes (subscribers resubscribe).
        "events.subscribe" => {
            if request["params"]["hold"] == true {
                std::thread::sleep(std::time::Duration::from_millis(1300)); // past the 1 s read tick
            }
            vec![ok(json!({})), json!({"event": "workspace.created", "data": {}})]
        }
        "agent.focus" if request["params"]["target"] == "fail" => {
            vec![json!({"id": id, "error": {"message": "no agent"}})]
        }
        _ => vec![ok(json!({}))],
    }
}

fn serve(stream: UnixStream) {
    let mut writer = stream.try_clone().expect("clone fake herdr stream");
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let Ok(request) = serde_json::from_str::<Value>(&line) else { return };
        let subscribe = request["method"] == "events.subscribe";
        for reply in answer(&request) {
            if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
                return;
            }
        }
        if subscribe {
            return;
        }
    }
}

/// The fake socket, started once per test process.
pub fn socket() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("space-kit-fake-herdr-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind fake herdr");
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                std::thread::spawn(move || serve(stream));
            }
        });
        path
    })
    .clone()
}
