//! Group edits: immutable `Groups` op → atomic save → rebuild → native sync. A failed save keeps
//! the old groups.
use super::app::{App, Prompt};
use super::keys::Action;
use crate::model::groups::{Groups, PathKey, UNGROUPED};
use crate::model::sidebar::Sidebar;
use crate::model::tree::{Kind, Row, Target};

/// Name of the nearest Group row at or above `cursor`.
pub fn group_at(rows: &[Row], cursor: usize) -> Option<&str> {
    let row = rows.get(..=cursor)?.iter().rev().find(|r| r.kind == Kind::Group)?;
    row.key.as_deref()?.strip_prefix("g:")
}

/// The top-level Space owning the row at `cursor` (None for group rows).
pub fn space_at(rows: &[Row], cursor: usize) -> Option<&Row> {
    rows.get(..=cursor)?.iter().rev().find(|r| r.depth <= 1).filter(|r| r.depth == 1)
}

impl App {
    pub(super) fn edit(&mut self, action: Action) {
        if let Some(buf) = self.prompt_buf() {
            match action {
                Action::Char(c) => return buf.push(c),
                Action::Backspace => {
                    buf.pop();
                    return;
                }
                _ => {}
            }
        }
        let Some(groups) = self.groups.clone() else {
            return self.flash("groups not loaded; edits disabled");
        };
        let group = group_at(&self.rows, self.cursor).filter(|g| *g != UNGROUPED).map(str::to_owned);
        match (action, group) {
            (Action::Submit, _) => self.submit(groups),
            (Action::NewGroup, _) => self.prompt = Some(Prompt::NewGroup(String::new())),
            (Action::Rename, Some(old)) => self.prompt = Some(Prompt::Rename { buf: old.clone(), old }),
            (Action::Delete, Some(name)) => self.prompt = Some(Prompt::Delete(name)),
            (Action::ReorderUp, Some(name)) => self.commit(groups.reorder(&name, -1), None, format!("moved {name} up")),
            (Action::ReorderDown, Some(name)) => {
                self.commit(groups.reorder(&name, 1), None, format!("moved {name} down"))
            }
            (Action::Move, _) => self.open_move_picker(&groups),
            (Action::Clean, _) => self.clean(&groups),
            (Action::Rename | Action::Delete | Action::ReorderUp | Action::ReorderDown, None) => {
                self.flash("no group under cursor")
            }
            _ => {}
        }
    }

    fn prompt_buf(&mut self) -> Option<&mut String> {
        match &mut self.prompt {
            Some(Prompt::NewGroup(buf) | Prompt::Rename { buf, .. }) => Some(buf),
            _ => None,
        }
    }

    fn open_move_picker(&mut self, groups: &Groups) {
        let Some(row) = space_at(&self.rows, self.cursor) else {
            return self.flash("move: put the cursor on a Space (worktrees follow their repo)");
        };
        let Some(Target::Workspace(id)) = &row.target else { return };
        let Some(ws) = self.snap.workspaces.iter().find(|w| &w.workspace_id == id) else { return };
        let path_key = PathKey::for_workspace(ws, &self.snap.panes);
        let current = groups.group_of(&path_key).unwrap_or(UNGROUPED);
        let options: Vec<String> = groups.groups.iter().map(|g| g.name.clone()).chain([UNGROUPED.to_owned()]).collect();
        let idx = options.iter().position(|o| o == current).unwrap_or(0);
        let label = ws.label.clone();
        self.prompt = Some(Prompt::Move { path_key: path_key.as_str().to_owned(), label, options, idx });
    }

    fn clean(&mut self, groups: &Groups) {
        let (cleaned, removed) = groups.clean(|p| p.exists());
        match removed.len() {
            0 => self.note("clean: nothing to remove"),
            n => self.commit(cleaned, None, format!("clean: removed {n} assignment(s)")),
        }
    }

    fn submit(&mut self, groups: Groups) {
        let Some(prompt) = self.prompt.take() else { return };
        let sidebar = &self.folds.sidebar;
        let result = match prompt {
            Prompt::NewGroup(name) => groups.create(&name).map(|g| (g, None, format!("created {}", name.trim()))),
            Prompt::Rename { old, buf } => groups
                .rename(&old, &buf)
                .map(|g| (g, Some(sidebar.renamed(&old, buf.trim())), format!("renamed {old} → {}", buf.trim()))),
            Prompt::Delete(name) => Ok((groups.delete(&name), None, format!("deleted {name}"))),
            Prompt::Move { path_key, label, options, idx } => {
                let target = options.get(idx).map(String::as_str).unwrap_or(UNGROUPED);
                let group = (target != UNGROUPED).then_some(target);
                Ok((groups.move_to(&path_key, group), None, format!("{label} → {target}")))
            }
        };
        match result {
            Ok((g, sidebar, msg)) => self.commit(g, sidebar, msg),
            Err(e) => self.flash(e),
        }
    }

    fn commit(&mut self, groups: Groups, sidebar: Option<Sidebar>, msg: String) {
        if let Err(e) = groups.save(&self.state_dir) {
            return self.flash(format!("save groups.json: {e}"));
        }
        // The groups are saved either way; a failed sidebar.json save must stay on screen.
        let sidebar_error = sidebar.as_ref().and_then(|s| s.save(&self.state_dir).err());
        if let Some(sidebar) = sidebar {
            self.folds.sidebar = sidebar;
        }
        self.groups = Some(groups);
        self.rebuild();
        match sidebar_error {
            Some(e) => self.flash(format!("{msg}, but sidebar.json: {e}")),
            None => self.note(msg),
        }
        self.sync_native();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tree::Tint;

    fn row(kind: Kind, depth: u8, key: &str) -> Row {
        Row {
            kind,
            depth,
            text: String::new(),
            tint: Tint::Plain,
            is_focused: false,
            collapsible: false,
            collapsed: false,
            key: Some(key.to_owned()),
            target: Some(Target::Workspace(key.to_owned())),
        }
    }

    fn rows() -> Vec<Row> {
        vec![
            row(Kind::Group, 0, "g:MADP"),
            row(Kind::Space, 1, "s:/a"),
            row(Kind::Agent, 2, "a1"),
            row(Kind::Worktree, 2, "s:/a-wt"),
            row(Kind::Group, 0, "g:Ungrouped"),
            row(Kind::Worktree, 1, "s:/orphan"),
        ]
    }

    #[test]
    fn group_at_finds_nearest_header_above() {
        let rows = rows();
        assert_eq!(group_at(&rows, 0), Some("MADP"));
        assert_eq!(group_at(&rows, 3), Some("MADP"));
        assert_eq!(group_at(&rows, 5), Some("Ungrouped"));
        assert_eq!(group_at(&rows, 99), None);
    }

    #[test]
    fn space_at_resolves_children_to_their_space() {
        let rows = rows();
        assert_eq!(space_at(&rows, 2).and_then(|r| r.key.as_deref()), Some("s:/a"));
        assert_eq!(space_at(&rows, 3).and_then(|r| r.key.as_deref()), Some("s:/a"));
        assert!(space_at(&rows, 0).is_none(), "group row");
        assert_eq!(
            space_at(&rows, 5).and_then(|r| r.key.as_deref()),
            Some("s:/orphan"),
            "an orphan worktree moves its repo"
        );
    }

    #[test]
    fn new_group_and_rename_prompts_save_groups_and_folds() {
        let dir = crate::native::report::tests::temp_dir("edit");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(dir.clone(), tx);
        app.groups = Some(Groups::default());
        app.handle(Action::NewGroup);
        "Work".chars().for_each(|c| {
            app.handle(Action::Char(c));
        });
        app.handle(Action::Submit);
        let saved = Groups::load(&dir, &Default::default()).unwrap();
        assert_eq!(saved.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["Work"]);
        assert!(app.prompt.is_none());

        app.folds.sidebar = Sidebar { collapsed: vec!["Work".into()], ..Default::default() };
        app.prompt = Some(Prompt::Rename { old: "Work".into(), buf: "Job".into() });
        app.handle(Action::Submit);
        assert_eq!(Groups::load(&dir, &Default::default()).unwrap().groups[0].name, "Job");
        assert_eq!(Sidebar::load(&dir).collapsed, ["Job"], "a folded group stays folded after rename");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn move_delete_reorder_and_clean_flows_save_groups() {
        use crate::tui::app::tests::app;
        let mut app = app("edit-flows");
        let saved = |app: &App| Groups::load(&app.state_dir, &Default::default()).unwrap();

        // m on `notes` (Ungrouped) → pick Work → it joins the group.
        app.cursor = 5;
        app.handle(Action::Move);
        assert!(matches!(&app.prompt, Some(Prompt::Move { label, idx: 1, .. }) if label == "notes"));
        app.step(-1);
        app.handle(Action::Submit);
        assert_eq!(saved(&app).assignments.values().filter(|g| *g == "Work").count(), 2);
        assert_eq!(app.status_text(), Some("notes → Work"));

        // A second group, reordered above Work, then deleted.
        app.handle(Action::NewGroup);
        "Home".chars().for_each(|c| {
            app.handle(Action::Char(c));
        });
        app.handle(Action::Backspace);
        app.handle(Action::Char('e'));
        app.handle(Action::Submit);
        assert_eq!(saved(&app).groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["Work", "Home"]);
        app.cursor = 0; // Work (empty Home is not drawn)
        app.handle(Action::ReorderDown);
        assert_eq!(saved(&app).groups[0].name, "Home");
        app.handle(Action::ReorderUp); // the cursor followed Work
        assert_eq!(saved(&app).groups[0].name, "Work");
        assert_eq!(app.status_text(), Some("moved Work up"));
        app.cursor = 0;
        app.handle(Action::Delete);
        assert_eq!(app.mode(), crate::tui::keys::Mode::Confirm);
        app.handle(Action::Submit);
        assert_eq!(saved(&app).groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["Home"]);
        assert!(saved(&app).assignments.is_empty(), "its members became Ungrouped");
        app.handle(Action::Clean);
        assert_eq!(app.status_text(), Some("clean: nothing to remove"));
    }

    #[test]
    fn clean_drops_assignments_whose_path_is_gone() {
        let mut app = crate::tui::app::tests::app("edit-clean");
        app.handle(Action::Clean);
        assert_eq!(app.status_text(), Some("clean: removed 1 assignment(s)"));
        assert!(Groups::load(&app.state_dir, &Default::default()).unwrap().assignments.is_empty());
    }

    #[test]
    fn edits_need_a_group_or_loaded_groups() {
        use crate::tui::app::tests::app;
        let mut app = app("edit-refusals");
        app.cursor = 4; // Ungrouped
        app.handle(Action::Rename);
        assert_eq!(app.status_text(), Some("no group under cursor"));
        app.cursor = 0;
        app.handle(Action::Move);
        assert!(app.status_text().unwrap().starts_with("move: put the cursor on a Space"));
        app.prompt = Some(Prompt::NewGroup("Work".into()));
        app.handle(Action::Submit);
        assert_eq!(app.status_text(), Some("group \"Work\" already exists"));
        app.groups = None;
        app.handle(Action::NewGroup);
        assert_eq!(app.status_text(), Some("groups not loaded; edits disabled"));
    }
}
