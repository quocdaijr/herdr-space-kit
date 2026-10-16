//! Agents tab grouped by workspace, through pane tokens: the first agent of each workspace
//! carries an `agent_header` line, every agent one `agent_line` cell, and `agent_sort` plus an
//! agent view sorted by it keeps the panel in sidebar order.
use super::tokens::Tokens;
use crate::herdr::types::Agent;
use crate::text::{clip, width};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const SORT_TOKEN: &str = "agent_sort";
pub const NAMES: [&str; 3] = ["agent_header", "agent_line", SORT_TOKEN];
/// Herdr indents rows after an entry's first by two columns; header-less agents pad to match.
const PAD: &str = "⠀⠀";

pub fn header_text(title: &str, max: usize) -> String {
    let text = format!("━ {} ", clip(title, max.saturating_sub(4).max(1)));
    let fill = max.saturating_sub(width(&text)).max(2);
    text + &"━".repeat(fill)
}

/// Pane id → tokens. `order` is workspace ids in sidebar order; `titles` their header text.
pub fn tokens(order: &[&str], agents: &[Agent], pane_order: &[&str], titles: &BTreeMap<&str, String>, max: usize) -> BTreeMap<String, Tokens> {
    let rank: BTreeMap<&str, usize> = pane_order.iter().enumerate().map(|(i, p)| (*p, i)).collect();
    let mut out = BTreeMap::new();
    for (ws_index, id) in order.iter().enumerate() {
        let mut members: Vec<&Agent> = agents.iter().filter(|a| a.workspace_id == *id).collect();
        members.sort_by_key(|a| rank.get(a.pane_id.as_str()).copied().unwrap_or(usize::MAX));
        for (i, agent) in members.iter().enumerate() {
            let first = i == 0;
            let name = if agent.agent.is_empty() { "agent" } else { &agent.agent };
            let title = agent.terminal_title_stripped.as_deref().unwrap_or("").trim();
            let text = match title {
                "" => format!("{} {name}", agent.agent_status.glyph()),
                _ => format!("{} {name}  {title}", agent.agent_status.glyph()),
            };
            let header = first.then(|| header_text(titles.get(id).map(String::as_str).unwrap_or(id), max));
            let line = format!("{}{}", if first { "" } else { PAD }, clip(&text, max.saturating_sub(2).max(1)));
            let tokens = BTreeMap::from([
                ("agent_header".to_owned(), header),
                ("agent_line".to_owned(), Some(line)),
                (SORT_TOKEN.to_owned(), Some(format!("{ws_index:04}-{i:03}"))),
            ]);
            out.insert(agent.pane_id.clone(), tokens);
        }
    }
    out
}

/// The agent view that sorts the panel by `agent_sort`; its label is the panel title.
pub fn view_params(source: &str, label: &str) -> Value {
    json!({"source": source, "label": label, "sort": [{"field": {"token": SORT_TOKEN}, "order": "asc"}]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::types::AgentStatus;

    fn agent(pane: &str, ws: &str, status: AgentStatus) -> Agent {
        Agent { pane_id: pane.into(), workspace_id: ws.into(), agent: "claude".into(), agent_status: status, terminal_title_stripped: Some("task".into()), ..Default::default() }
    }

    #[test]
    fn first_agent_per_workspace_carries_the_header_and_sort_follows_order() {
        let agents = [agent("p2", "w1", AgentStatus::Working), agent("p1", "w1", AgentStatus::Idle), agent("p5", "w2", AgentStatus::Idle)];
        let titles = BTreeMap::from([("w1", "MADP › web-app".to_owned()), ("w2", "MADP › web-app › worktree-calm".to_owned())]);
        let t = tokens(&["w1", "w2"], &agents, &["p1", "p2", "p5"], &titles, 30);
        assert!(t["p1"]["agent_header"].as_deref().unwrap().starts_with("━ MADP › web-app ━"));
        assert_eq!(t["p2"]["agent_header"], None);
        assert!(t["p5"]["agent_header"].as_deref().unwrap().starts_with("━ MADP › web-app › worktree…"));
        assert_eq!(t["p1"]["agent_line"].as_deref(), Some("○ claude  task"));
        assert_eq!(t["p2"]["agent_line"].as_deref(), Some("⠀⠀● claude  task"));
        assert_eq!(t["p2"]["agent_sort"].as_deref(), Some("0000-001"));
        assert_eq!(t["p5"]["agent_sort"].as_deref(), Some("0001-000"));
        assert_eq!(width(&header_text("notes", 20)), 20);
    }
}
