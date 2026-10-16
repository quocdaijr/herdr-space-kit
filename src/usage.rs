//! Claude / Codex rate-limit usage for the Agents panel title.
//!
//! Herdr has no usage data of its own. Claude Code's statusline script pipes its JSON to
//! `space-kit record`, which caches `rate_limits` in ~/.cache/claude-rate-limits.json; Codex
//! writes `rate_limits` into its session logs. The text goes in the agent view label (the panel
//! title): it never scrolls and is never highlighted, unlike rows, which belong to an agent entry.
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_LABEL: &str = "by space";
const CODEX_TAIL_BYTES: u64 = 256 * 1024;

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn claude_cache() -> PathBuf {
    home().join(".cache/claude-rate-limits.json")
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// "42%" for one limit window ("0%" once it has reset); None without a usage figure.
pub fn percent(window: &Value, now: f64) -> Option<String> {
    let used = window.get("used_percentage").or_else(|| window.get("used_percent"))?.as_f64()?;
    if window["resets_at"].as_f64().is_some_and(|resets| resets <= now) {
        return Some("0%".into());
    }
    Some(format!("{}%", used.round_ties_even() as i64))
}

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            jsonl_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        _ => true,
    }
}

/// `rate_limits` of the last event that has one in the newest Codex session log.
pub fn latest_codex_limits(root: &Path) -> Option<Value> {
    let mut logs = Vec::new();
    jsonl_files(root, &mut logs);
    let newest = logs.into_iter().max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())?;
    let mut file = std::fs::File::open(newest).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(CODEX_TAIL_BYTES))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .filter(|line| line.contains("\"rate_limits\""))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|event| event["payload"]["rate_limits"].clone())
        .find(truthy)
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// e.g. "✻7% 42% ◆0%" (Claude 5h and 7d, then Codex); "by space" without data.
pub fn label_from(claude: &Value, codex: &Value, now: f64) -> String {
    let windows = |limits: &Value, keys: [&str; 2]| -> Vec<String> { keys.iter().filter_map(|k| percent(&limits[*k], now)).collect() };
    let mut parts = Vec::new();
    let claude = windows(&claude["rate_limits"], ["five_hour", "seven_day"]);
    if !claude.is_empty() {
        parts.push(format!("✻{}", claude.join(" ")));
    }
    let codex = windows(codex, ["primary", "secondary"]);
    if !codex.is_empty() {
        parts.push(format!("◆{}", codex.join(" ")));
    }
    if parts.is_empty() { DEFAULT_LABEL.into() } else { parts.join(" ") }
}

pub fn label() -> String {
    let codex = latest_codex_limits(&home().join(".codex/sessions")).unwrap_or(Value::Null);
    label_from(&read_json(&claude_cache()), &codex, now())
}

/// Save Claude's statusline `rate_limits` when a figure changed; true when it did.
pub fn record(statusline_input: &str, cache: &Path) -> std::io::Result<bool> {
    let Ok(input) = serde_json::from_str::<Value>(statusline_input) else { return Ok(false) };
    let limits = &input["rate_limits"];
    if !truthy(limits) || &read_json(cache)["rate_limits"] == limits {
        return Ok(false);
    }
    crate::model::write_atomic(cache, &json!({"rate_limits": limits}).to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::report::tests::temp_dir;

    const NOW: f64 = 1_000_000.0;

    #[test]
    fn percent_rounds_half_to_even_and_reads_zero_after_reset() {
        assert_eq!(percent(&json!({"used_percentage": 41.6, "resets_at": NOW + 60.0}), NOW).as_deref(), Some("42%"));
        assert_eq!(percent(&json!({"used_percentage": 42.5}), NOW).as_deref(), Some("42%"));
        assert_eq!(percent(&json!({"used_percent": 90, "resets_at": NOW - 1.0}), NOW).as_deref(), Some("0%"));
        assert_eq!(percent(&Value::Null, NOW), None);
    }

    #[test]
    fn label_joins_claude_and_codex() {
        let claude = json!({"rate_limits": {"five_hour": {"used_percentage": 7}, "seven_day": {"used_percentage": 42.4}}});
        let codex = json!({"primary": {"used_percent": 0, "window_minutes": 43200}});
        assert_eq!(label_from(&claude, &codex, NOW), "✻7% 42% ◆0%");
        assert_eq!(label_from(&claude, &Value::Null, NOW), "✻7% 42%");
        assert_eq!(label_from(&Value::Null, &Value::Null, NOW), "by space");
    }

    #[test]
    fn codex_limits_come_from_the_newest_log_tail() {
        let root = temp_dir("codex");
        let log = root.join("2026/10/16/rollout.jsonl");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let events: Vec<String> = [1, 2].iter().map(|p| json!({"payload": {"rate_limits": {"primary": {"used_percent": p}}}}).to_string()).collect();
        std::fs::write(&log, format!("{}\n{{bad json \"rate_limits\"\n", events.join("\n"))).unwrap();
        assert_eq!(latest_codex_limits(&root), Some(json!({"primary": {"used_percent": 2}})));
        assert_eq!(latest_codex_limits(&root.join("missing")), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_writes_only_when_a_figure_changes() {
        let root = temp_dir("record");
        let cache = root.join("c.json");
        let payload = json!({"rate_limits": {"five_hour": {"used_percentage": 7}}}).to_string();
        assert!(record(&payload, &cache).unwrap());
        assert!(!record(&payload, &cache).unwrap());
        assert!(!record(r#"{"model": {}}"#, &cache).unwrap());
        assert!(!record("not json", &cache).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }
}
