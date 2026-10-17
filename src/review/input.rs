use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::App;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    Quit,
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::Continue;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => app.scroll(1, 0),
        KeyCode::Up | KeyCode::Char('k') => app.scroll(-1, 0),
        KeyCode::Right | KeyCode::Char('l') => app.scroll(0, 4),
        KeyCode::Left | KeyCode::Char('h') => app.scroll(0, -4),
        KeyCode::PageDown => app.scroll(app.height.max(1) as isize, 0),
        KeyCode::PageUp => app.scroll(-(app.height.max(1) as isize), 0),
        KeyCode::Home | KeyCode::Char('g') => app.top = 0,
        KeyCode::End | KeyCode::Char('G') => app.top = app.max_top(),
        KeyCode::Tab => app.switch_file(false),
        KeyCode::BackTab => app.switch_file(true),
        _ => {}
    }
    Action::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::app::tests::fixture;

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
