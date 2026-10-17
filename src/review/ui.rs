use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, List, ListItem, ListState, Paragraph},
};

use super::app::App;

pub fn render(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());
    let package = &app.session.package;
    let source = package
        .source
        .as_ref()
        .map_or("local path", |source| source.repr.as_str());
    let heading = Paragraph::new(vec![
        Line::from(app.approval_status()),
        Line::from(format!("Source: {source} | {}", package.id)),
        Line::from(format!(
            "Approvals: {}",
            app.session.config.path().display()
        )),
    ])
    .block(Block::bordered().title(format!(
        " {} v{} — Read-only review ",
        package.name, package.version
    )));
    frame.render_widget(heading, header);

    let [files, source] = if body.width >= 80 {
        Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(body)
    } else {
        Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(body)
    };
    let items: Vec<_> = app
        .files
        .iter()
        .map(|file| ListItem::new(file.label.clone()))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" Files (Tab / Shift+Tab) "))
        .highlight_symbol("› ")
        .highlight_style(Style::default().fg(Color::Cyan));
    frame.render_stateful_widget(
        list,
        files,
        &mut ListState::default().with_selected(Some(app.selected)),
    );

    let block = Block::bordered().title(format!(" {} ", app.file().label));
    let inner = block.inner(source);
    frame.render_widget(block, source);
    let gutter_width = app.file().lines.len().max(1).to_string().len() as u16 + 2;
    let [gutter, code] =
        Layout::horizontal([Constraint::Length(gutter_width), Constraint::Min(0)]).areas(inner);
    app.set_viewport(code.height, code.width);
    let lines: Vec<_> = app
        .file()
        .lines
        .iter()
        .skip(app.top)
        .take(app.height)
        .cloned()
        .collect();
    let numbers: Vec<_> = (app.top..app.top + lines.len())
        .map(|line| {
            Line::from(format!(
                "{:>width$} ",
                line + 1,
                width = usize::from(gutter_width) - 1
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(numbers).style(Style::default().fg(Color::DarkGray)),
        gutter,
    );
    frame.render_widget(Paragraph::new(lines).scroll((0, app.left)), code);

    let position = if app.file().lines.is_empty() {
        "Empty file".into()
    } else {
        format!("Line {} / {}", app.top + 1, app.file().lines.len())
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(
                "↑/↓ j/k Scroll · ←/→ h/l Pan · PgUp/PgDn · Home/End · Tab Files · q/Esc Quit",
            ),
            Line::from(format!("{position} · Read-only: approvals are not changed")),
        ]),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::review::app::tests::fixture;

    #[test]
    fn renders_source_status_and_empty_files_at_small_sizes() {
        let (_root, mut app) = fixture(&["fn main() {\n    println!(\"hello\");\n}", ""]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("fn main()"));
        assert!(screen.contains("no previous approval"));
        assert!(screen.contains("Read-only review"));
        app.switch_file(false);
        for (width, height) in [(100, 24), (40, 12), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }
}
