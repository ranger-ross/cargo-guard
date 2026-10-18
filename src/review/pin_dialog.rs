use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Style},
    widgets::{Block, Clear, Paragraph, Wrap},
};

use super::{app::Decision, diagnostic::Diagnostic, input::Action};

pub(super) struct PinDialog {
    pub diagnostic: Diagnostic,
    pin_focused: bool,
    pin_button: Rect,
    cancel_button: Rect,
}

impl PinDialog {
    pub fn new(error: String) -> Self {
        let mut diagnostic = Diagnostic::default();
        diagnostic.set_report(error, true);
        Self {
            diagnostic,
            pin_focused: false,
            pin_button: Rect::default(),
            cancel_button: Rect::default(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::Continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if key.kind == KeyEventKind::Press {
            if key.code == KeyCode::BackTab
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
            {
                self.pin_focused = !self.pin_focused;
                return Action::Continue;
            }
            if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Char('p') => return Action::Decide(Decision::PinVersion),
                    KeyCode::Esc => return Action::CancelPin,
                    KeyCode::Char('q') => return Action::Quit,
                    KeyCode::Char('y') => return Action::CopySelection,
                    KeyCode::Enter => {
                        return if self.pin_focused {
                            Action::Decide(Decision::PinVersion)
                        } else {
                            Action::CancelPin
                        };
                    }
                    KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                        self.pin_focused = !self.pin_focused;
                    }
                    _ => {}
                }
            }
        }
        if key.modifiers.is_empty() {
            let page = isize::try_from(self.diagnostic.selection.area.height).unwrap_or(1);
            match key.code {
                KeyCode::Up => self.diagnostic.scroll(-1),
                KeyCode::Down => self.diagnostic.scroll(1),
                KeyCode::PageUp => self.diagnostic.scroll(-page),
                KeyCode::PageDown => self.diagnostic.scroll(page),
                KeyCode::Home => self.diagnostic.scroll(isize::MIN),
                KeyCode::End => self.diagnostic.scroll(isize::MAX),
                _ => {}
            }
        }
        Action::Continue
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> (bool, Action) {
        // A selection drag owns its release, including over a button.
        if self.diagnostic.selection.is_dragging() {
            let changed = self.diagnostic.handle_mouse(event).unwrap_or(false);
            return (changed, Action::Continue);
        }
        if let Some(changed) = self.diagnostic.handle_mouse(event) {
            return (changed, Action::Continue);
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) && event.modifiers.is_empty() {
            let position = Position::new(event.column, event.row);
            if self.pin_button.contains(position) {
                self.pin_focused = true;
                return (true, Action::Decide(Decision::PinVersion));
            }
            if self.cancel_button.contains(position) {
                return (true, Action::CancelPin);
            }
        }
        (false, Action::Continue)
    }

    pub fn render(&mut self, frame: &mut Frame, name: &str, version: &semver::Version) {
        let area = frame.area();
        let width = area.width.min(76);
        let height = area.height.min(16);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let block = Block::bordered()
            .border_style(Style::default().fg(Color::Red))
            .title(format!(" Fingerprint unavailable: {name} {version} "));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let [body, warning, buttons] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(inner);
        self.diagnostic.render_body(frame, body);
        frame.render_widget(
            Paragraph::new("Pinning permits any build-script contents for this version.")
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: false }),
            warning,
        );
        let [pin, _, cancel] = Layout::horizontal([
            Constraint::Length(17),
            Constraint::Length(2),
            Constraint::Length(16),
        ])
        .areas(buttons);
        self.pin_button = pin;
        self.cancel_button = cancel;
        let focused = Style::default().fg(Color::Black).bg(Color::LightBlue);
        frame.render_widget(
            Paragraph::new(" Pin version [p] ").style(if self.pin_focused {
                focused
            } else {
                Style::default()
            }),
            pin,
        );
        frame.render_widget(
            Paragraph::new(" Cancel [Esc] ").style(if self.pin_focused {
                Style::default()
            } else {
                focused
            }),
            cancel,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn cancel_is_default_and_only_explicit_presses_can_pin() {
        let mut dialog = PinDialog::new("fingerprint failed".into());
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Action::CancelPin);
        dialog.handle_key(key(KeyCode::Tab));
        for code in [KeyCode::Enter, KeyCode::Char('p')] {
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                assert_eq!(
                    dialog.handle_key(KeyEvent { kind, ..key(code) }),
                    Action::Continue
                );
            }
            for modifiers in [
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
            ] {
                assert_eq!(
                    dialog.handle_key(KeyEvent::new(code, modifiers)),
                    Action::Continue
                );
            }
            assert_eq!(
                dialog.handle_key(key(code)),
                Action::Decide(Decision::PinVersion)
            );
        }
        for code in ['a', 'f', 'r', 's', 'j', 'k', '?'] {
            assert_eq!(
                dialog.handle_key(key(KeyCode::Char(code))),
                Action::Continue
            );
        }
        dialog.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Action::CancelPin);
        assert_eq!(dialog.handle_key(key(KeyCode::Esc)), Action::CancelPin);
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('y'))),
            Action::CopySelection
        );
    }

    #[test]
    fn selection_drag_across_button_never_pins() {
        let report = "First error line\nSecond error line";
        let mut dialog = PinDialog::new(report.into());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, "example", &semver::Version::new(1, 0, 0)))
            .unwrap();
        let mouse = |kind, area: Rect| MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        dialog.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            dialog.diagnostic.selection.area,
        ));
        assert_eq!(
            dialog.handle_mouse(mouse(
                MouseEventKind::Drag(MouseButton::Left),
                dialog.pin_button
            )),
            (true, Action::Continue)
        );
        assert_eq!(
            dialog.handle_mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                dialog.pin_button
            )),
            (true, Action::Continue)
        );
        assert_eq!(
            dialog.diagnostic.selected_text().as_deref(),
            Some("First error line\nS")
        );
        assert_eq!(
            dialog.handle_mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                Rect::default()
            )),
            (false, Action::Continue)
        );
        assert_eq!(
            dialog.handle_mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                dialog.cancel_button
            )),
            (true, Action::CancelPin)
        );
        assert_eq!(
            dialog.handle_mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                dialog.pin_button
            )),
            (true, Action::Decide(Decision::PinVersion))
        );
    }
}
