//! Display-width helpers and agent state glyph counts for sidebar rows.
use crate::herdr::types::AgentStatus;
use unicode_width::UnicodeWidthChar;

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0).max(1)
}

pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Cut `text` to `max` display columns, ending in '…' when cut.
pub fn clip(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return "…".repeat(max);
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        if used + char_width(c) > max - 1 {
            break;
        }
        used += char_width(c);
        out.push(c);
    }
    out + "…"
}

/// prefix + name + suffix within `max`, shortening only the name so counts stay visible.
pub fn fit_row(prefix: &str, name: &str, suffix: &str, max: usize) -> String {
    let room = max.saturating_sub(width(prefix) + width(suffix)).max(1);
    format!("{prefix}{}{suffix}", clip(name, room))
}

const COUNTED: [AgentStatus; 4] = [AgentStatus::Blocked, AgentStatus::Working, AgentStatus::Done, AgentStatus::Idle];

/// Per-state agent counts in priority order, e.g. "  ●2 ○1"; "" without agents.
pub fn state_counts(statuses: &[AgentStatus]) -> String {
    let parts: Vec<String> = COUNTED
        .iter()
        .filter_map(|s| {
            let n = statuses.iter().filter(|x| *x == s).count();
            (n > 0).then(|| format!("{}{n}", s.glyph()))
        })
        .collect();
    if parts.is_empty() { String::new() } else { format!("  {}", parts.join(" ")) }
}

/// Working and blocked counts for a folded group header, e.g. " ●2 ?1".
pub fn rollup(statuses: &[AgentStatus]) -> String {
    let parts: Vec<String> = [AgentStatus::Working, AgentStatus::Blocked]
        .iter()
        .filter_map(|s| {
            let n = statuses.iter().filter(|x| *x == s).count();
            (n > 0).then(|| format!("{}{n}", s.glyph()))
        })
        .collect();
    if parts.is_empty() { String::new() } else { format!(" {}", parts.join(" ")) }
}

/// Usable sidebar columns: SPACE_KIT_ROW_WIDTH, else `ui.sidebar_width` from Herdr's config minus
/// the border (Herdr's default width is 26).
pub fn row_width() -> usize {
    const DEFAULT: i64 = 26;
    const MIN: usize = 10;
    if let Some(n) = std::env::var("SPACE_KIT_ROW_WIDTH").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let configured = std::fs::read_to_string(home.join(".config/herdr/config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| t.get("ui")?.get("sidebar_width")?.as_integer())
        .unwrap_or(DEFAULT);
    usize::try_from(configured - 1).unwrap_or(0).max(MIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentStatus::*;

    #[test]
    fn counts_follow_priority_and_skip_missing() {
        assert_eq!(state_counts(&[Idle, Working, Working]), "  ●2 ○1");
        assert_eq!(state_counts(&[Unknown]), "");
        assert_eq!(rollup(&[Working, Idle, Blocked, Working]), " ●2 ?1");
        assert_eq!(rollup(&[Idle]), "");
    }

    #[test]
    fn rows_shorten_the_name_not_the_counts() {
        assert_eq!(fit_row("├─ ", "app-loyalty-theme-extension", "  ●1", 20), "├─ app-loyalty-…  ●1");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abc", 1), "…");
        assert_eq!(clip("abc", 0), "");
        assert_eq!(width("漢a"), 3);
    }
}
