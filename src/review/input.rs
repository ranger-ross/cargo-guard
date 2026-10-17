use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::App;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    Quit,
    Approve,
    Reject,
    NextPackage,
    PreviousPackage,
    Skip,
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::Continue;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    if key.code == KeyCode::Char('?') && key.kind == KeyEventKind::Press {
        app.show_help = !app.show_help;
        return Action::Continue;
    }
    if app.show_help {
        match key.code {
            KeyCode::Esc => app.show_help = false,
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
        return Action::Continue;
    }
    if matches!(key.code, KeyCode::Char('a' | 'r')) && !app.can_decide() {
        return Action::Continue;
    }
    match key.code {
        KeyCode::Char('n') if app.queue_mode => return Action::NextPackage,
        KeyCode::Char('p') if app.queue_mode => return Action::PreviousPackage,
        KeyCode::Char('s') if app.queue_mode && key.kind == KeyEventKind::Press => {
            return Action::Skip;
        }
        KeyCode::Char('a') if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
            return Action::Approve;
        }
        KeyCode::Char('r') if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
            return Action::Reject;
        }
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => app.scroll(1, 0),
        KeyCode::Up | KeyCode::Char('k') => app.scroll(-1, 0),
        KeyCode::Right | KeyCode::Char('l') => app.scroll(0, 4),
        KeyCode::Left | KeyCode::Char('h') => app.scroll(0, -4),
        KeyCode::PageDown => app.scroll(app.height.max(1) as isize, 0),
        KeyCode::PageUp => app.scroll(-(app.height.max(1) as isize), 0),
        KeyCode::Home | KeyCode::Char('g') => app.scroll(isize::MIN, 0),
        KeyCode::End | KeyCode::Char('G') => app.scroll(isize::MAX, 0),
        KeyCode::Tab => app.switch_file(false),
        KeyCode::BackTab => app.switch_file(true),
        KeyCode::Char(']') => app.jump_change(false),
        KeyCode::Char('[') => app.jump_change(true),
        _ => {}
    }
    Action::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::app::tests::fixture;

    #[test]
    fn help_blocks_decisions_and_fingerprint_actions_are_disabled() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        handle_key(&mut app, key('?'));
        assert!(app.show_help);
        assert_eq!(handle_key(&mut app, key('a')), Action::Continue);
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.show_help);
        assert_eq!(handle_key(&mut app, key('a')), Action::Approve);
        app.session.package.approval =
            Some(crate::config::ApprovedVersion::Fingerprint("hash".into()));
        for c in ['a', 'r'] {
            assert_eq!(handle_key(&mut app, key(c)), Action::Continue);
        }
    }

    #[test]
    fn approved_versions_ignore_approval_and_rejection_keys() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        app.session.package.approval = Some(crate::config::ApprovedVersion::SemVer(
            app.session.package.version.clone(),
        ));
        for character in ['a', 'r'] {
            assert_eq!(
                handle_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
                ),
                Action::Continue
            );
        }
    }

    #[test]
    fn approval_actions_require_an_unmodified_key_press() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        for (character, action) in [('a', Action::Approve), ('r', Action::Reject)] {
            let key = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
            assert_eq!(handle_key(&mut app, key), action);
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                assert_eq!(
                    handle_key(&mut app, KeyEvent { kind, ..key }),
                    Action::Continue
                );
            }
            assert_eq!(
                handle_key(
                    &mut app,
                    KeyEvent {
                        modifiers: KeyModifiers::CONTROL,
                        ..key
                    }
                ),
                Action::Continue
            );
        }
        assert!(!app.session.config.path().exists());
    }

    #[test]
    fn handles_navigation_quit_and_key_release() {
        let (_root, mut app) = fixture(&["a\nb\nc\nd", "other"]);
        app.set_viewport(2, 10);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        handle_key(&mut app, key(KeyCode::PageDown));
        assert_eq!(app.top, 2);
        handle_key(&mut app, key(KeyCode::Home));
        assert_eq!(app.top, 0);
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.selected, 1);
        handle_key(&mut app, key(KeyCode::BackTab));
        assert_eq!(app.selected, 0);
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(handle_key(&mut app, release), Action::Continue);
        assert_eq!(handle_key(&mut app, key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Action::Quit
        );
    }
}
