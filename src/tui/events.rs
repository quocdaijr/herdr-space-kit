//! Subscription threads: one connection per subscription set, push-only after the ack.
//! A: lifecycle events, always on. B: `pane.agent_status_changed` for the current agent panes,
//! respawned whenever that set changes.
use crate::herdr::client::socket_path;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

const LIFECYCLE: [&str; 24] = [
    "workspace.created",
    "workspace.updated",
    "workspace.metadata_updated",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.closed",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
    "layout.updated",
];
const RETRY: Duration = Duration::from_secs(1);
/// Read timeout, so a superseded status thread notices within a second.
const TICK: Duration = Duration::from_secs(1);
const READ_CAPACITY: usize = 1 << 20;

/// Each `()` sent on `tx` means something changed (an event, or a resubscribe): refetch.
pub fn spawn_lifecycle(tx: Sender<()>) {
    let subs: Vec<Value> = LIFECYCLE.iter().map(|t| json!({"type": t})).collect();
    std::thread::spawn(move || run(&json!({"subscriptions": subs}), &tx, &|| true));
}

/// Owner of thread B. `set` restarts it when the pane set changes; an empty set stops it.
#[derive(Default)]
pub struct StatusSub {
    generation: Arc<AtomicU64>,
    panes: Vec<String>,
}

impl StatusSub {
    pub fn set(&mut self, panes: Vec<String>, tx: &Sender<()>) {
        if panes == self.panes {
            return;
        }
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let subs: Vec<Value> =
            panes.iter().map(|p| json!({"type": "pane.agent_status_changed", "pane_id": p})).collect();
        self.panes = panes;
        if subs.is_empty() {
            return;
        }
        let (generation, tx) = (self.generation.clone(), tx.clone());
        std::thread::spawn(move || {
            run(&json!({"subscriptions": subs}), &tx, &|| generation.load(Ordering::SeqCst) == mine)
        });
    }
}

/// Subscribe until `alive` turns false; on EOF/error/`events_lost` wait 1 s, resubscribe, invalidate.
fn run(params: &Value, tx: &Sender<()>, alive: &dyn Fn() -> bool) {
    while alive() {
        if let Err(e) = stream(params, tx, alive) {
            super::log(&format!("subscription: {e}"));
        }
        if !alive() {
            return;
        }
        std::thread::sleep(RETRY);
        if tx.send(()).is_err() {
            return;
        }
    }
}

fn stream(params: &Value, tx: &Sender<()>, alive: &dyn Fn() -> bool) -> Result<(), String> {
    let mut sock = UnixStream::connect(socket_path()).map_err(|e| format!("connect: {e}"))?;
    sock.set_read_timeout(Some(TICK)).map_err(|e| e.to_string())?;
    let req = json!({"id": "space-kit:subscribe", "method": "events.subscribe", "params": params});
    writeln!(sock, "{req}").map_err(|e| format!("write: {e}"))?;
    let mut reader = BufReader::with_capacity(READ_CAPACITY, sock);
    let mut buf = Vec::new();
    while alive() {
        match reader.read_until(b'\n', &mut buf) {
            Ok(_) if buf.ends_with(b"\n") => {
                let msg: Value = serde_json::from_slice(&buf).map_err(|e| format!("bad json: {e}"))?;
                buf.clear();
                if let Some(err) = msg.get("error") {
                    return Err(err.to_string()); // includes `events_lost`
                }
                if msg["event"].is_string() && tx.send(()).is_err() {
                    return Ok(());
                }
            }
            Ok(_) => return Err("connection closed".into()),
            // Timeout keeps the partial line in `buf`; read_until resumes appending.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(())
}
