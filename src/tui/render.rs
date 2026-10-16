//! Manager rendering: one line per tree row, a status line, help and the move picker.
use super::app::{App, Prompt};
use crate::herdr::types::AgentStatus;
use crate::model::tree::{Kind, Row, Tint};
use crate::text::{clip, width};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

const CURSOR_BG: Color = rgb(0x313244);
const DIM: Color = rgb(0x9a9eb3);
const HELP: [&str; 15] = [
    "j/k ↑↓  move",
    "Enter   jump and close",
    "Space   fold/unfold",
    "h/l     fold / unfold",
    "H / L   fold groups / unfold all",
    "n       new group",
    "r       rename group",
    "d       delete group",
    "m       move Space to a group",
    "J/K     reorder group",
    "c       clean stale paths",
    "g       refresh",
    "?       help",
    "q/Esc   close",
    "click · 2×click jump",
];

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn group_color(name: &str) -> Color {
    rgb(match name {
        "green" => 0xa6e3a1,
        "blue" => 0x89b4fa,
        "mauve" => 0xcba6f7,
        "peach" => 0xfab387,
        "yellow" => 0xf9e2af,
        "teal" => 0x94e2d5,
        "red" => 0xf38ba8,
        _ => 0x9a9eb3,
    })
}

fn status_color(status: AgentStatus) -> Color {
    rgb(match status {
        AgentStatus::Blocked => 0xf38ba8,
        AgentStatus::Working => 0xf9e2af,
        AgentStatus::Done => 0x94e2d5,
        AgentStatus::Idle => 0x9a9eb3,
        AgentStatus::Unknown => 0x585b70,
    })
}

fn row_line(row: &Row, is_cursor: bool, is_marked: bool, w: usize) -> Line<'static> {
    let mut style = match &row.tint {
        Tint::Group(name) => Style::new().fg(group_color(name)),
        Tint::Status(s) => Style::new().fg(status_color(*s)),
        Tint::Grey => Style::new().fg(DIM),
        Tint::Plain => Style::new(),
    };
    if row.kind == Kind::Group {
        style = style.add_modifier(Modifier::BOLD);
    }
    if is_marked {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if is_cursor {
        style = style.bg(CURSOR_BG);
    }
    // Space/Worktree rows carry no arrow; mark a folded one at the end.
    let folded = row.collapsed && row.kind != Kind::Group;
    let text = match folded {
        true => clip(&row.text, w.saturating_sub(2)) + " ▸",
        false => clip(&row.text, w),
    };
    let pad = " ".repeat(w.saturating_sub(width(&text)));
    Line::from(Span::styled(text + &pad, style))
}

/// Keep the cursor inside the `height`-row window.
pub fn clamp_scroll(scroll: usize, cursor: usize, len: usize, height: usize) -> usize {
    let scroll = scroll.min(cursor).max((cursor + 1).saturating_sub(height));
    scroll.min(len.saturating_sub(height))
}

pub fn list_height(area: Rect) -> usize {
    usize::from(area.height.saturating_sub(1))
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [list, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let h = usize::from(list.height);
    app.scroll = clamp_scroll(app.scroll, app.cursor, app.rows.len(), h);
    let w = usize::from(list.width);
    // Focus marker: the focused agent row, or else the focused Space.
    let agent_focused = app.rows.iter().any(|r| r.kind == Kind::Agent && r.is_focused);
    let lines: Vec<Line> = app
        .rows
        .iter()
        .enumerate()
        .skip(app.scroll)
        .take(h)
        .map(|(i, r)| row_line(r, i == app.cursor, r.is_focused && (r.kind == Kind::Agent || !agent_focused), w))
        .collect();
    f.render_widget(Paragraph::new(lines), list);
    f.render_widget(Paragraph::new(status_line(app, w)), status);
    if let Some(Prompt::Move { label, options, idx, .. }) = &app.prompt {
        draw_picker(f, list, label, options, *idx);
    }
    if app.help {
        let lines: Vec<Line> = HELP.iter().map(|l| Line::raw(clip(l, w.saturating_sub(2)))).collect();
        f.render_widget(Clear, list);
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" keys ")), list);
    }
}

fn status_line(app: &App, w: usize) -> Line<'static> {
    let text = match &app.prompt {
        Some(Prompt::NewGroup(buf)) => format!("new: {buf}▏"),
        Some(Prompt::Rename { buf, .. }) => format!("rename: {buf}▏"),
        Some(Prompt::Delete(name)) => format!("delete {name}? y/n"),
        Some(Prompt::Move { .. }) => "j/k Enter · Esc".to_owned(),
        None => match app.status_text() {
            Some(msg) => msg.to_owned(),
            None => {
                return Line::styled(
                    clip("n new · m move · Space fold · Enter jump · ? help", w),
                    Style::new().fg(DIM),
                );
            }
        },
    };
    // Keep the end of a long input visible (by display columns, so wide chars never overflow).
    let mut shown = text;
    while width(&shown) > w {
        shown.remove(0);
    }
    Line::styled(shown, Style::new().fg(rgb(0xf9e2af)))
}

fn draw_picker(f: &mut Frame, list: Rect, label: &str, options: &[String], idx: usize) {
    let height = (options.len() as u16 + 2).min(list.height);
    let area = Rect { y: list.bottom().saturating_sub(height), height, ..list };
    let inner = usize::from(area.width.saturating_sub(2));
    let lines: Vec<Line> = options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let style = if i == idx { Style::new().bg(CURSOR_BG).add_modifier(Modifier::BOLD) } else { Style::new() };
            Line::styled(clip(o, inner), style)
        })
        .collect();
    let title = format!(" move {} ", clip(label, inner.saturating_sub(7)));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_the_tree_and_the_status_hint() {
        use crate::herdr::types::{Snapshot, Workspace};
        use crate::model::groups::Groups;
        use ratatui::{Terminal, backend::TestBackend};
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(std::env::temp_dir().join("space-kit-render"), tx);
        let ws =
            |id: &str, label: &str| Workspace { workspace_id: id.into(), label: label.into(), ..Default::default() };
        app.snap = Snapshot {
            workspaces: vec![ws("h", "⟦Work⟧"), ws("a", "api"), ws("b", "notes")],
            ..Default::default()
        };
        app.groups = Some(Groups::default().create("Work").unwrap().move_to("label:api", Some("Work")));
        app.rebuild();
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let line = |y: u16| (0..40).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>();
        assert_eq!(line(0).trim_end(), "▾ Work ·1");
        assert_eq!(line(1).trim_end(), "    api");
        assert_eq!(line(2).trim_end(), "▾ Ungrouped ·1");
        assert!(line(5).starts_with("n new · m move"), "{}", line(5));
    }

    #[test]
    fn scroll_follows_cursor() {
        assert_eq!(clamp_scroll(0, 0, 100, 10), 0);
        assert_eq!(clamp_scroll(0, 15, 100, 10), 6);
        assert_eq!(clamp_scroll(20, 5, 100, 10), 5);
        assert_eq!(clamp_scroll(50, 99, 100, 10), 90);
        assert_eq!(clamp_scroll(5, 3, 4, 10), 0, "short lists never scroll");
    }

    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn status_line_shows_prompts_and_keeps_the_end_of_long_input() {
        let mut app = crate::tui::app::tests::app("render-prompts");
        app.prompt = Some(Prompt::NewGroup("Work".into()));
        assert_eq!(screen(&mut app, 30, 8)[7], "new: Work▏");
        app.prompt =
            Some(Prompt::Rename { old: "Work".into(), buf: "漢字漢字漢字漢字漢字漢字漢字".into() });
        // The test buffer stores the second half of a wide char as a blank cell; drop those.
        let last = screen(&mut app, 12, 8)[7].replace(' ', "");
        assert!(last.ends_with("字▏") && width(&last) <= 12, "{last}");
        app.prompt = Some(Prompt::Delete("Work".into()));
        assert_eq!(screen(&mut app, 30, 8)[7], "delete Work? y/n");
        app.prompt = None;
        app.note("saved");
        assert_eq!(screen(&mut app, 30, 8)[7], "saved");
    }

    #[test]
    fn help_and_move_picker_draw_over_the_list() {
        let mut app = crate::tui::app::tests::app("render-overlays");
        app.help = true;
        let help = screen(&mut app, 40, 20).join("\n");
        assert!(help.contains(" keys ") && help.contains("Enter   jump and close"), "{help}");
        app.help = false;
        app.prompt = Some(Prompt::Move {
            path_key: "/x".into(),
            label: "notes".into(),
            options: vec!["Work".into(), "Ungrouped".into()],
            idx: 1,
        });
        let picker = screen(&mut app, 40, 12).join("\n");
        assert!(
            picker.contains(" move notes ") && picker.contains("Ungrouped") && picker.contains("j/k Enter · Esc"),
            "{picker}"
        );
    }

    #[test]
    fn a_folded_space_is_marked_at_the_end() {
        let mut app = crate::tui::app::tests::app("render-folded");
        app.cursor = 1;
        app.handle(crate::tui::keys::Action::Collapse);
        assert_eq!(screen(&mut app, 30, 8)[1], "  ● api ▸");
    }
}
