use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{
    app::{App, Decision},
    diagnostic::Diagnostic,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    Quit,
    Decide(Decision),
    NextPackage,
    PreviousPackage,
    Skip,
    CopySelection,
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
        app.help.reset_view();
        app.selection.clear();
        return Action::Continue;
    }
    if app.show_help {
        return handle_help_key(&mut app.show_help, &mut app.help, key);
    }
    if matches!(key.code, KeyCode::Char('a' | 'r')) && !app.can_decide() {
        return Action::Continue;
    }
    match key.code {
        KeyCode::Char('j') if app.queue_mode => return Action::NextPackage,
        KeyCode::Char('k') if app.queue_mode => return Action::PreviousPackage,
        KeyCode::Char('s') if app.queue_mode && key.kind == KeyEventKind::Press => {
            return Action::Skip;
        }
        KeyCode::Char('a') if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
            return Action::Decide(Decision::Approve);
        }
        KeyCode::Char('r') if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
            return Action::Decide(Decision::Reject);
        }
        KeyCode::Char('y') if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
            return Action::CopySelection;
        }
        KeyCode::Esc if app.selection.clear() => {}
        KeyCode::Esc if app.diagnostic.selection.clear() => {}
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Down => app.scroll(1, 0),
        KeyCode::Up => app.scroll(-1, 0),
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

pub(super) fn handle_help_key(
    show_help: &mut bool,
    help: &mut Diagnostic,
    key: KeyEvent,
) -> Action {
    match key.code {
        KeyCode::Esc => *show_help = false,
        KeyCode::Char('q') => return Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => help.scroll(1),
        KeyCode::Up | KeyCode::Char('k') => help.scroll(-1),
        KeyCode::PageDown => help.scroll(help.selection.area.height.max(1) as isize),
        KeyCode::PageUp => help.scroll(-(help.selection.area.height.max(1) as isize)),
        KeyCode::Home | KeyCode::Char('g') => help.scroll(isize::MIN),
        KeyCode::End | KeyCode::Char('G') => help.scroll(isize::MAX),
        _ => {}
    }
    Action::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::app::tests::fixture;

    #[test]
    fn accept_pins_the_version_and_help_blocks_decisions() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        handle_key(&mut app, key('?'));
        assert!(app.show_help);
        assert_eq!(handle_key(&mut app, key('a')), Action::Continue);
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.show_help);
        assert_eq!(
            handle_key(&mut app, key('a')),
            Action::Decide(Decision::Approve)
        );
        assert_eq!(handle_key(&mut app, key('f')), Action::Continue);
        assert_eq!(handle_key(&mut app, key('p')), Action::Continue);
        assert_eq!(
            handle_key(&mut app, key('r')),
            Action::Decide(Decision::Reject)
        );
    }

    #[test]
    fn approved_versions_and_blankets_ignore_approval_and_rejection_keys() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        for approval in [
            crate::config::ApprovedVersion::SemVer(app.session.package.version.clone()),
            crate::config::ApprovedVersion::Blanket(crate::config::BlanketApproval::Approved),
        ] {
            app.session.package.approval = Some(approval);
            for character in ['a', 'r', 'f'] {
                assert_eq!(
                    handle_key(
                        &mut app,
                        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
                    ),
                    Action::Continue
                );
            }
        }
    }

    #[test]
    fn approval_actions_require_an_unmodified_key_press() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        for (character, action) in [
            ('a', Action::Decide(Decision::Approve)),
            ('r', Action::Decide(Decision::Reject)),
        ] {
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
    fn package_navigation_does_not_scroll_source() {
        let (_root, mut app) = fixture(&["a\nb\nc\nd\ne\nf"]);
        app.set_viewport(2, 10);
        app.scroll(1, 0);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        for queue_mode in [false, true] {
            app.queue_mode = queue_mode;
            for (character, action) in [('j', Action::NextPackage), ('k', Action::PreviousPackage)]
            {
                assert_eq!(
                    handle_key(&mut app, key(character)),
                    if queue_mode { action } else { Action::Continue }
                );
                assert_eq!(app.top, 1);
            }
            for character in ['n', 'p'] {
                assert_eq!(handle_key(&mut app, key(character)), Action::Continue);
                assert_eq!(app.top, 1);
            }
        }
        for (code, top) in [
            (KeyCode::Down, 2),
            (KeyCode::Up, 1),
            (KeyCode::PageDown, 3),
            (KeyCode::PageUp, 1),
        ] {
            assert_eq!(
                handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE)),
                Action::Continue
            );
            assert_eq!(app.top, top);
        }
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
