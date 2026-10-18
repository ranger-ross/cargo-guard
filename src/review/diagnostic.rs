use std::ops::Range;

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style},
    widgets::{Block, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::selection::Selection;

#[derive(Default)]
pub(super) struct Diagnostic {
    report: String,
    rows: Vec<Range<usize>>,
    width: u16,
    top: usize,
    is_error: bool,
    pub selection: Selection,
}

impl Diagnostic {
    pub fn set_report(&mut self, report: String, is_error: bool) {
        self.is_error = is_error;
        if self.report == report {
            return;
        }
        self.report = report;
        self.rows.clear();
        self.width = 0;
        self.top = 0;
        self.selection.clear();
        self.selection.area = Rect::default();
    }

    pub fn reset_view(&mut self) {
        self.top = 0;
        self.selection.clear();
        self.selection.area = Rect::default();
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection.bounds()?;
        let first = self.rows.get(start.row)?;
        let last = self.rows.get(end.row)?;
        let start = first.start + byte_column(&self.report[first.clone()], start.column);
        let end = last.start + byte_column(&self.report[last.clone()], end.end);
        Some(self.report[start..end].to_owned())
    }

    pub fn height(&mut self, width: u16, available: u16) -> u16 {
        if self.report.is_empty() {
            return 0;
        }
        self.wrap(width.saturating_sub(2));
        let height = self.rows.len().saturating_add(2).min(8) as u16;
        height.min(available.saturating_sub(4).max(3).min(available))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if self.report.is_empty() {
            self.selection.area = Rect::default();
            return;
        }
        let color = if self.is_error {
            Color::Red
        } else {
            Color::Yellow
        };
        let title = if self.is_error {
            " Error "
        } else {
            " Warning "
        };
        let block = Block::bordered()
            .border_style(Style::default().fg(color))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.render_body(frame, inner);
    }

    pub fn render_body(&mut self, frame: &mut Frame, inner: Rect) {
        self.wrap(inner.width);
        self.selection.area = inner;
        self.top = self.top.min(self.max_top());
        for (offset, range) in self
            .rows
            .iter()
            .skip(self.top)
            .take(usize::from(inner.height))
            .enumerate()
        {
            let text = &self.report[range.clone()];
            let row = Rect::new(inner.x, inner.y + offset as u16, inner.width, 1);
            frame.render_widget(Paragraph::new(text), row);
            if let Some(columns) = self.selection.columns(self.top + offset) {
                let end = columns
                    .end
                    .min(text.width() + 1)
                    .min(usize::from(inner.width));
                for column in columns.start.min(end)..end {
                    frame.buffer_mut()[(inner.x + column as u16, row.y)]
                        .set_style(Style::default().fg(Color::Black).bg(Color::LightBlue));
                }
            }
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<bool> {
        if self.report.is_empty() || event.modifiers.contains(KeyModifiers::SHIFT) {
            return None;
        }
        let inside = self
            .selection
            .area
            .contains(Position::new(event.column, event.row));
        if !inside && !self.selection.is_dragging() {
            return None;
        }
        let scrolling = inside
            && matches!(
                event.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            );
        let mut event = event;
        if scrolling {
            self.scroll(if event.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            });
            if self.selection.is_dragging() {
                event.kind = MouseEventKind::Drag(MouseButton::Left);
            }
        }
        let changed = self
            .selection
            .handle_mouse(event, self.top, 0, self.rows.len(), |row| {
                self.report[self.rows[row].clone()].graphemes(true)
            });
        Some(scrolling || changed)
    }

    fn max_top(&self) -> usize {
        self.rows
            .len()
            .saturating_sub(usize::from(self.selection.area.height).max(1))
    }

    pub fn scroll(&mut self, rows: isize) {
        self.top = self.top.saturating_add_signed(rows).min(self.max_top());
    }

    fn wrap(&mut self, width: u16) {
        let width = width.max(1);
        if width == self.width {
            return;
        }
        self.width = width;
        self.rows = wrap_ranges(&self.report, usize::from(width));
        self.selection.clear();
        self.top = self.top.min(self.max_top());
    }
}

fn byte_column(text: &str, column: usize) -> usize {
    let mut cells = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        if cells >= column {
            return index;
        }
        cells += grapheme.width();
    }
    text.len()
}

fn wrap_ranges(text: &str, width: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let mut base = 0;
    for line in text.split('\n') {
        let mut start = 0;
        if line.is_empty() {
            rows.push(base..base);
        }
        while start < line.len() {
            let mut cells = 0;
            let mut end = start;
            let mut word_break = None;
            for (index, grapheme) in line[start..].grapheme_indices(true) {
                let next = cells + grapheme.width();
                if next > width && end > start {
                    break;
                }
                cells = next;
                end = start + index + grapheme.len();
                if grapheme.chars().all(char::is_whitespace) {
                    word_break = Some(end);
                }
            }
            if end < line.len() {
                end = word_break.unwrap_or(end);
            }
            rows.push(base + start..base + end);
            start = end;
        }
        base += line.len() + 1;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_preserves_whitespace_and_unicode_boundaries() {
        let text = "long/path/without/spaces\n\n  spaced words \u{754c}e\u{301}";
        for width in [1, 3, 12, 80] {
            let rows = wrap_ranges(text, width);
            let joined: String = rows.iter().map(|range| &text[range.clone()]).collect();
            assert_eq!(joined, text.replace('\n', ""));
            assert!(rows.iter().all(
                |range| text.is_char_boundary(range.start) && text.is_char_boundary(range.end)
            ));
        }
    }

    #[test]
    fn copied_selection_preserves_real_newlines_without_soft_wraps() {
        let mut diagnostic = Diagnostic::default();
        let report = "No artifacts for /a/long/path/build.rs\n\nBuild the matching target.";
        diagnostic.set_report(report.into(), true);
        diagnostic.wrap(12);
        diagnostic.selection.area = Rect::new(2, 2, 12, 30);
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        diagnostic.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 2));
        diagnostic.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 13, 31));
        diagnostic.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 13, 31));
        assert_eq!(diagnostic.selected_text().as_deref(), Some(report));
        diagnostic.set_report(report.into(), true);
        assert_eq!(diagnostic.selected_text().as_deref(), Some(report));
        diagnostic.set_report("Different error".into(), true);
        assert!(diagnostic.selected_text().is_none());
    }
}
