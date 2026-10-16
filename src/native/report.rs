//! Report only what changed since the last sync (cached in reported.json). Status changes run a
//! sync often, and resending ~40 workspaces each time is wasted work.
use super::agents::NAMES as AGENT_TOKENS;
use super::tokens::Tokens;
use crate::herdr::client::{Api, Result};
use crate::model::write_atomic;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

pub const SOURCE: &str = "plugin:space-kit";
/// Herdr rejects a whole report past this many tokens; reports from one source merge.
pub const MAX_TOKENS_PER_REPORT: usize = 16;
const CACHE: &str = "reported.json";

type Cache = BTreeMap<String, Tokens>;

fn report(api: &dyn Api, method: &str, target: (&str, &str), tokens: &Tokens) -> Result<()> {
    let items: Vec<_> = tokens.iter().collect();
    for chunk in items.chunks(MAX_TOKENS_PER_REPORT) {
        let part: BTreeMap<_, _> = chunk.iter().copied().collect();
        api.call(method, json!({target.0: target.1, "source": SOURCE, "tokens": part}))?;
    }
    Ok(())
}

/// `tokens`, plus a clear for every token reported last time that is gone now.
fn with_clears(tokens: &Tokens, previous: Option<&Tokens>) -> Tokens {
    let mut out = tokens.clone();
    for name in previous.into_iter().flat_map(|p| p.keys()).filter(|k| !tokens.contains_key(*k)) {
        out.insert(name.clone(), None);
    }
    out
}

/// Tokens are lost on a server restart, so startup passes `force`. Returns true on a full resend.
pub fn changed(api: &dyn Api, state_dir: &Path, workspaces: &Cache, panes: &Cache, force: bool) -> Result<bool> {
    let path = state_dir.join(CACHE);
    let cache: Cache =
        std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    // Forced: resend everything, but still clear tokens the cache says were reported before.
    let unchanged = |key: &str, tokens: &Tokens| !force && cache.get(key) == Some(tokens);
    let mut sent = Cache::new();
    for (id, tokens) in workspaces {
        if !unchanged(id, tokens) {
            report(api, "workspace.report_metadata", ("workspace_id", id), &with_clears(tokens, cache.get(id)))?;
        }
        sent.insert(id.clone(), tokens.clone());
    }
    for (pane, tokens) in panes {
        let key = format!("pane:{pane}");
        if !unchanged(&key, tokens) {
            report(api, "pane.report_metadata", ("pane_id", pane), &with_clears(tokens, cache.get(&key)))?;
        }
        sent.insert(key, tokens.clone());
    }
    let cleared: Tokens = AGENT_TOKENS.iter().map(|n| (n.to_string(), None)).collect();
    for key in cache.keys().filter(|k| k.starts_with("pane:") && !sent.contains_key(*k)) {
        // The agent left this pane (or it closed): clear its lines once; a closed pane errors.
        let _ = report(api, "pane.report_metadata", ("pane_id", &key["pane:".len()..]), &cleared);
    }
    if let Ok(text) = serde_json::to_string(&sent) {
        // Only saves work: a failed write means a full resend next time.
        let _ = write_atomic(&path, &text);
    }
    Ok(force || cache.is_empty())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::Value;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct Fake {
        pub sent: RefCell<Vec<(String, Value)>>,
        pub snapshot: Value,
    }

    impl Api for Fake {
        fn call(&self, method: &str, params: Value) -> Result<Value> {
            self.sent.borrow_mut().push((method.to_owned(), params));
            Ok(if method == "session.snapshot" { json!({"snapshot": self.snapshot}) } else { json!({}) })
        }
    }

    impl Fake {
        pub fn methods(&self) -> Vec<String> {
            self.sent.borrow().iter().map(|(m, _)| m.clone()).collect()
        }
    }

    pub fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("space-kit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn one(id: &str, pairs: &[(&str, Option<&str>)]) -> Cache {
        let tokens = pairs.iter().map(|(k, v)| (k.to_string(), v.map(str::to_owned))).collect();
        BTreeMap::from([(id.to_owned(), tokens)])
    }

    #[test]
    fn sends_changes_clears_dropped_tokens_and_departed_panes() {
        let dir = temp_dir("report");
        let api = Fake::default();
        let panes = one("p1", &[("agent_line", Some("x"))]);
        assert!(
            changed(&api, &dir, &one("w1", &[("old", Some("1")), ("group_row", Some("a"))]), &panes, false).unwrap()
        );

        api.sent.borrow_mut().clear();
        assert!(!changed(&api, &dir, &one("w1", &[("group_row", Some("a"))]), &Cache::new(), false).unwrap());
        let sent = api.sent.borrow();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].1["tokens"], json!({"group_row": "a", "old": null}), "a dropped token is cleared");
        assert_eq!(sent[1].1["pane_id"], "p1");
        assert_eq!(sent[1].1["tokens"]["agent_line"], Value::Null);
        drop(sent);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_forced_resend_still_clears_tokens_from_the_cache() {
        let dir = temp_dir("force");
        let api = Fake::default();
        changed(&api, &dir, &one("w1", &[("old", Some("1"))]), &Cache::new(), false).unwrap();
        api.sent.borrow_mut().clear();
        assert!(changed(&api, &dir, &one("w1", &[("group_row", Some("a"))]), &Cache::new(), true).unwrap());
        assert_eq!(api.sent.borrow()[0].1["tokens"], json!({"group_row": "a", "old": null}));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_are_chunked_by_sixteen() {
        let dir = temp_dir("chunks");
        let api = Fake::default();
        let tokens: Tokens = (0..20).map(|i| (format!("t{i:02}"), Some("x".to_owned()))).collect();
        changed(&api, &dir, &BTreeMap::from([("w1".to_owned(), tokens)]), &Cache::new(), true).unwrap();
        let sizes: Vec<usize> = api.sent.borrow().iter().map(|(_, p)| p["tokens"].as_object().unwrap().len()).collect();
        assert_eq!(sizes, [16, 4]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
