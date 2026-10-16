//! Pure key map: `(KeyEvent, Mode) -> Action`.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Help,
    /// Single-line text prompt (new / rename).
    Input,
    /// `y/n` question (delete).
    Confirm,
    /// List picker (move).
    Pick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Up,
    Down,
    Jump,
    Toggle,
    Collapse,
    Expand,
    CollapseAll,
    ExpandAll,
    NewGroup,
    Rename,
    Delete,
    Move,
    ReorderUp,
    ReorderDown,
    Clean,
    Refresh,
    Help,
    Quit,
    Char(char),
    Backspace,
    Submit,
    Cancel,
}

pub fn map(key: KeyEvent, mode: Mode) -> Action {
    use Action::*;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match mode {
        Mode::Normal => match key.code {
            KeyCode::Char('j') | KeyCode::Down => Down,
            KeyCode::Char('k') | KeyCode::Up => Up,
            KeyCode::Enter => Jump,
            KeyCode::Char(' ') => Toggle,
            KeyCode::Char('h') | KeyCode::Left => Collapse,
            KeyCode::Char('l') | KeyCode::Right => Expand,
            KeyCode::Char('H') => CollapseAll,
            KeyCode::Char('L') => ExpandAll,
            KeyCode::Char('n') => NewGroup,
            KeyCode::Char('r') => Rename,
            KeyCode::Char('d') => Delete,
            KeyCode::Char('m') => Move,
            KeyCode::Char('K') => ReorderUp,
            KeyCode::Char('J') => ReorderDown,
            KeyCode::Char('c') => Clean,
            KeyCode::Char('g') => Refresh,
            KeyCode::Char('?') => Help,
            KeyCode::Char('q') | KeyCode::Esc => Quit,
            _ => None,
        },
        // Any key closes help.
        Mode::Help => Help,
        Mode::Input => match key.code {
            KeyCode::Enter => Submit,
            KeyCode::Esc => Cancel,
            KeyCode::Backspace => Backspace,
            KeyCode::Char(c) => Char(c),
            _ => None,
        },
        Mode::Confirm => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Submit,
            _ => Cancel,
        },
        Mode::Pick => match key.code {
            KeyCode::Char('j') | KeyCode::Down => Down,
            KeyCode::Char('k') | KeyCode::Up => Up,
            KeyCode::Enter => Submit,
            KeyCode::Esc | KeyCode::Char('q') => Cancel,
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn normal_mode_keys() {
        let cases = [
            ('j', Action::Down),
            ('k', Action::Up),
            (' ', Action::Toggle),
            ('h', Action::Collapse),
            ('l', Action::Expand),
            ('H', Action::CollapseAll),
            ('L', Action::ExpandAll),
            ('n', Action::NewGroup),
            ('r', Action::Rename),
            ('d', Action::Delete),
            ('m', Action::Move),
            ('J', Action::ReorderDown),
            ('K', Action::ReorderUp),
            ('c', Action::Clean),
            ('g', Action::Refresh),
            ('?', Action::Help),
            ('q', Action::Quit),
            ('x', Action::None),
        ];
        for (c, want) in cases {
            assert_eq!(map(k(KeyCode::Char(c)), Mode::Normal), want, "key {c:?}");
        }
        assert_eq!(map(k(KeyCode::Enter), Mode::Normal), Action::Jump);
        assert_eq!(map(k(KeyCode::Down), Mode::Normal), Action::Down);
        assert_eq!(map(k(KeyCode::Up), Mode::Normal), Action::Up);
    }

    #[test]
    fn input_mode_types_letters_instead_of_commands() {
        assert_eq!(map(k(KeyCode::Char('q')), Mode::Input), Action::Char('q'));
        assert_eq!(map(k(KeyCode::Char('j')), Mode::Input), Action::Char('j'));
        assert_eq!(map(k(KeyCode::Enter), Mode::Input), Action::Submit);
        assert_eq!(map(k(KeyCode::Esc), Mode::Input), Action::Cancel);
        assert_eq!(map(k(KeyCode::Backspace), Mode::Input), Action::Backspace);
    }

    #[test]
    fn confirm_only_y_submits() {
        assert_eq!(map(k(KeyCode::Char('y')), Mode::Confirm), Action::Submit);
        assert_eq!(map(k(KeyCode::Char('n')), Mode::Confirm), Action::Cancel);
        assert_eq!(map(k(KeyCode::Enter), Mode::Confirm), Action::Cancel);
    }

    #[test]
    fn pick_and_help_modes() {
        assert_eq!(map(k(KeyCode::Char('j')), Mode::Pick), Action::Down);
        assert_eq!(map(k(KeyCode::Enter), Mode::Pick), Action::Submit);
        assert_eq!(map(k(KeyCode::Esc), Mode::Pick), Action::Cancel);
        assert_eq!(map(k(KeyCode::Char('x')), Mode::Help), Action::Help);
    }

    #[test]
    fn ctrl_chords_are_ignored() {
        let ev = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(map(ev, Mode::Normal), Action::None);
    }
}
