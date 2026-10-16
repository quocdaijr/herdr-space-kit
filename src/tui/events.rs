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
            super::log(&crate::model::state_dir(), &format!("subscription: {e}"));
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
                if !forward(&std::mem::take(&mut buf), tx)? {
                    return Ok(());
                }
            }
            Ok(_) => return Err("connection closed".into()),
            Err(e) => keep_waiting(e)?,
        }
    }
    Ok(())
}

/// Pass one pushed line on as a refetch. False when nobody listens any more; an error reply
/// (`events_lost` included) ends the subscription.
fn forward(line: &[u8], tx: &Sender<()>) -> Result<bool, String> {
    let msg: Value = serde_json::from_slice(line).map_err(|e| format!("bad json: {e}"))?;
    if let Some(err) = msg.get("error") {
        return Err(err.to_string());
    }
    Ok(!msg["event"].is_string() || tx.send(()).is_ok())
}

/// A read timeout just means no event yet (the partial line stays buffered); anything else ends.
fn keep_waiting(e: std::io::Error) -> Result<(), String> {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => Ok(()),
        _ => Err(format!("read: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    #[test]
    fn lines_are_forwarded_and_errors_end_the_subscription() {
        let (tx, rx) = mpsc::channel();
        assert_eq!(forward(br#"{"event":"pane.closed"}"#, &tx), Ok(true));
        assert_eq!(forward(br#"{"id":"x","result":{}}"#, &tx), Ok(true), "the ack is not an event");
        assert_eq!(rx.try_iter().count(), 1);
        assert!(forward(br#"{"error":{"code":"events_lost"}}"#, &tx).unwrap_err().contains("events_lost"));
        assert!(forward(b"nope", &tx).unwrap_err().starts_with("bad json"));
        drop(rx);
        assert_eq!(forward(br#"{"event":"pane.closed"}"#, &tx), Ok(false), "nobody listens");
        assert_eq!(keep_waiting(ErrorKind::TimedOut.into()), Ok(()));
        assert_eq!(keep_waiting(ErrorKind::WouldBlock.into()), Ok(()));
        assert!(keep_waiting(ErrorKind::ConnectionReset.into()).unwrap_err().starts_with("read:"));
    }

    #[test]
    fn a_subscription_relays_events_then_resubscribes_after_a_hangup() {
        let (tx, rx) = mpsc::channel();
        let passes = AtomicUsize::new(0);
        // Alive for two streams: event + hang-up, the retry ping, a second event + hang-up.
        let alive = || passes.fetch_add(1, Ordering::SeqCst) < 5;
        run(&json!({"subscriptions": []}), &tx, &alive);
        assert!(rx.try_iter().count() >= 2, "events and the resubscribe ping arrive");
        assert_eq!(stream(&json!({}), &tx, &|| false), Ok(()), "a dead subscriber reads nothing");
        drop(rx);
        let ping_fails = AtomicUsize::new(0);
        run(&json!({}), &tx, &|| ping_fails.fetch_add(1, Ordering::SeqCst) < 4); // send fails: stops
    }

    #[test]
    fn status_subscriptions_follow_the_agent_panes() {
        let (tx, _rx) = mpsc::channel();
        let mut sub = StatusSub::default();
        sub.set(vec!["a:p1".into()], &tx);
        let first = sub.generation.load(Ordering::SeqCst);
        sub.set(vec!["a:p1".into()], &tx);
        assert_eq!(sub.generation.load(Ordering::SeqCst), first, "same panes: nothing restarts");
        sub.set(Vec::new(), &tx);
        assert_eq!(sub.generation.load(Ordering::SeqCst), first + 1, "the old thread is told to stop");
        spawn_lifecycle(tx);
    }

    #[test]
    fn a_quiet_subscription_waits_through_read_timeouts() {
        let (tx, rx) = mpsc::channel();
        assert_eq!(stream(&json!({"hold": true}), &tx, &|| true), Err("connection closed".into()));
        assert_eq!(rx.try_iter().count(), 1, "the event after the quiet spell arrives");
    }
}
