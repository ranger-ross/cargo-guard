use std::ops::Range;

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    app::App,
    diff::{DiffLine, LineKind},
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Point {
    pub row: usize,
    pub column: usize,
    pub end: usize,
}

#[derive(Default)]
pub(super) struct Selection {
    pub area: Rect,
    anchor: Option<Point>,
    cursor: Option<Point>,
    dragging: bool,
}

impl Selection {
    pub fn clear(&mut self) -> bool {
        let active = self.anchor.take().is_some();
        self.cursor = None;
        self.dragging = false;
        active
    }

    pub fn is_active(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn bounds(&self) -> Option<(Point, Point)> {
        let anchor = self.anchor?;
        let cursor = self.cursor?;
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    pub fn handle_mouse<'a, I: Iterator<Item = &'a str>>(
        &mut self,
        event: MouseEvent,
        top: usize,
        left: u16,
        count: usize,
        graphemes: impl Fn(usize) -> I,
    ) -> bool {
        let inside = self.area.contains(Position::new(event.column, event.row));
        let point = || {
            if self.area.is_empty() || count == 0 {
                return None;
            }
            let row = (top
                + usize::from(event.row.clamp(self.area.y, self.area.bottom() - 1) - self.area.y))
            .min(count - 1);
            let column = usize::from(left)
                + usize::from(event.column.clamp(self.area.x, self.area.right() - 1) - self.area.x);
            let mut start = 0;
            for grapheme in graphemes(row) {
                let end = start + grapheme.width();
                if end > column {
                    return Some(Point {
                        row,
                        column: start,
                        end,
                    });
                }
                start = end;
            }
            Some(Point {
                row,
                column: start,
                end: start,
            })
        };
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let point = inside.then(point).flatten();
                let changed = self.clear();
                if let Some(point) = point {
                    self.anchor = Some(point);
                    self.cursor = Some(point);
                    self.dragging = true;
                    return true;
                }
                changed
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                if self.dragging =>
            {
                self.cursor = point();
                self.dragging = !matches!(event.kind, MouseEventKind::Up(_));
                true
            }
            _ => false,
        }
    }

    pub fn columns(&self, row: usize) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        let cursor = self.cursor?;
        let (start, end) = (anchor.min(cursor), anchor.max(cursor));
        if row < start.row || row > end.row {
            return None;
        }
        Some(
            if row == start.row { start.column } else { 0 }..if row == end.row {
                end.end
            } else {
                usize::MAX
            },
        )
    }

    pub fn text(&self, rows: &[DiffLine]) -> Option<String> {
        let anchor = self.anchor?;
        let cursor = self.cursor?;
        let mut text = String::new();
        let mut first = true;
        for (index, row) in rows
            .iter()
            .enumerate()
            .take(anchor.row.max(cursor.row) + 1)
            .skip(anchor.row.min(cursor.row))
        {
            if row.kind == LineKind::Note {
                continue;
            }
            if !first {
                text.push('\n');
            }
            first = false;
            let columns = self.columns(index)?;
            let mut column = 0;
            for grapheme in graphemes(row) {
                let end = column + grapheme.width();
                if end > columns.start && column < columns.end {
                    text.push_str(grapheme);
                }
                column = end;
            }
        }
        Some(text)
    }
}

impl App {
    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        if self.show_help || event.modifiers.contains(KeyModifiers::SHIFT) {
            return false;
        }
        if let Some(changed) = self.diagnostic.handle_mouse(event) {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.selection.clear();
            }
            return changed;
        }
        let cleared = event.kind == MouseEventKind::Down(MouseButton::Left)
            && self.diagnostic.selection.clear();
        let inside = self
            .selection
            .area
            .contains(Position::new(event.column, event.row));
        if inside
            && matches!(
                event.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            self.scroll(
                if event.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                },
                0,
            );
            if !self.selection.is_dragging() {
                return true;
            }
            let event = MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                ..event
            };
            let rows = &self.files[self.selected].rows;
            self.selection
                .handle_mouse(event, self.top, self.left, rows.len(), |row| {
                    graphemes(&rows[row])
                });
            return true;
        }
        let rows = &self.files[self.selected].rows;
        let changed = self
            .selection
            .handle_mouse(event, self.top, self.left, rows.len(), |row| {
                graphemes(&rows[row])
            });
        cleared || changed
    }
}

fn graphemes(row: &DiffLine) -> impl Iterator<Item = &str> {
    row.text
        .spans
        .iter()
        .flat_map(|span| span.content.graphemes(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::app::tests::fixture;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn drag(app: &mut App, from: (u16, u16), to: (u16, u16)) {
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            from.0,
            from.1,
        ));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1));
    }

    #[test]
    fn selection_wraps_within_source_and_preserves_interior_lines() {
        let (_root, mut app) = fixture(&["abcdef\n\nghijkl"]);
        app.selection.area = Rect::new(30, 2, 20, 10);
        drag(&mut app, (32, 2), (32, 4));
        assert_eq!(
            app.selection.text(&app.file().rows).as_deref(),
            Some("cdef\n\nghi")
        );
        drag(&mut app, (32, 4), (32, 2));
        assert_eq!(
            app.selection.text(&app.file().rows).as_deref(),
            Some("cdef\n\nghi")
        );
        drag(&mut app, (32, 2), (0, 4));
        assert_eq!(
            app.selection.text(&app.file().rows).as_deref(),
            Some("cdef\n\ng")
        );
        drag(&mut app, (0, 2), (32, 4));
        assert!(!app.selection.is_active());
    }

    #[test]
    fn selection_accounts_for_scroll_and_grapheme_cells() {
        let (_root, mut app) = fixture(&["ignored\n012345\na\u{754c}e\u{301}z\nlast"]);
        app.set_viewport(2, 4);
        app.scroll(1, 2);
        app.selection.area = Rect::new(30, 2, 4, 2);
        drag(&mut app, (30, 2), (31, 3));
        assert_eq!(
            app.selection.text(&app.file().rows).as_deref(),
            Some("2345\na\u{754c}e\u{301}")
        );
        drag(&mut app, (30, 3), (30, 3));
        assert_eq!(
            app.selection.text(&app.file().rows).as_deref(),
            Some("\u{754c}")
        );
    }

    #[test]
    fn source_replacement_and_file_switch_clear_selection() {
        let (_root, mut app) = fixture(&["first", "second"]);
        app.selection.area = Rect::new(30, 2, 20, 10);
        drag(&mut app, (30, 2), (32, 2));
        app.switch_file(false);
        assert!(!app.selection.is_active());
        drag(&mut app, (30, 2), (32, 2));
        app.set_baseline(super::super::sources::Baseline::CurrentVersion)
            .unwrap();
        assert!(!app.selection.is_active());
    }

    #[test]
    fn copy_omits_diff_annotations_and_escape_clears_without_quitting() {
        use crate::review::input::{Action, handle_key};
        use crossterm::event::{KeyCode, KeyEvent};

        let (_root, mut app) = fixture(&["first\nlast"]);
        app.selection.area = Rect::new(30, 2, 20, 10);
        drag(&mut app, (30, 2), (49, 11));
        assert_eq!(
            app.selection.text(&app.file().rows).as_deref(),
            Some("first\nlast")
        );
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(handle_key(&mut app, escape), Action::Continue);
        assert!(!app.selection.is_active());
        assert_eq!(handle_key(&mut app, escape), Action::Quit);
    }

    #[test]
    fn help_and_shift_drag_do_not_select_underlying_source() {
        let (_root, mut app) = fixture(&["first"]);
        app.selection.area = Rect::new(30, 2, 20, 10);
        app.show_help = true;
        drag(&mut app, (30, 2), (32, 2));
        assert!(!app.selection.is_active());
        app.show_help = false;
        let mut event = mouse(MouseEventKind::Down(MouseButton::Left), 30, 2);
        event.modifiers = KeyModifiers::SHIFT;
        app.handle_mouse(event);
        assert!(!app.selection.is_active());
        app.selection.area = Rect::default();
        drag(&mut app, (30, 2), (32, 2));
        assert!(!app.selection.is_active());
    }
}
