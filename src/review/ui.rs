use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::app::App;
use super::diff::FileStatus;
use super::queue::QueueState;
use super::sources::Baseline;

pub fn render(frame: &mut Frame, app: &mut App) {
    render_area(frame, app, frame.area());
}

pub fn render_queue(frame: &mut Frame, state: &mut QueueState) {
    let [packages, review] = if frame.area().width >= 120 {
        Layout::horizontal([Constraint::Length(36), Constraint::Min(0)]).areas(frame.area())
    } else {
        Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(frame.area())
    };
    let items: Vec<_> = state
        .entries
        .iter()
        .map(|entry| {
            ListItem::new(vec![
                Line::from(format!("{} v{}", entry.package.name, entry.package.version)),
                Line::from(format!("  [{}]", entry.status())),
            ])
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(format!(
                    " Packages {}/{} · n/p ",
                    state.selected + 1,
                    state.entries.len()
                ))
                .title_bottom(" s Skip · q Summary "),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().fg(Color::Cyan));
    frame.render_stateful_widget(
        list,
        packages,
        &mut ListState::default().with_selected(Some(state.selected)),
    );
    if let Some(app) = &mut state.app {
        render_area(frame, app, review);
    } else {
        let entry = &state.entries[state.selected];
        frame.render_widget(Paragraph::new(format!(
            "{}\n\nCould not load sources:\n{}\n\nn/p: another package · s: skip · q: exit with summary",
            entry.package.id, entry.error.as_deref().unwrap_or("unknown error")
        )).wrap(Wrap { trim: false }).block(Block::bordered().title(" Unresolved package ")), review);
    }
}

fn render_area(frame: &mut Frame, app: &mut App, area: Rect) {
    let unavailable = matches!(app.baseline, Baseline::Unavailable { .. });
    let [header, notice, baseline, body, footer] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(if app.notice.is_some() { 3 } else { 0 }),
        Constraint::Length(if unavailable { 4 } else { 1 }),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(area);
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
        " {} v{} — Build script review ",
        package.name, package.version
    )));
    frame.render_widget(heading, header);
    if let Some(message) = &app.notice {
        frame.render_widget(
            Paragraph::new(message.message.as_str())
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(if message.is_error {
                    Color::Red
                } else {
                    Color::Green
                })),
            notice,
        );
    }
    let baseline_style = Style::default().fg(if unavailable {
        Color::Yellow
    } else {
        Color::DarkGray
    });
    frame.render_widget(
        Paragraph::new(app.baseline_message())
            .wrap(Wrap { trim: false })
            .style(baseline_style),
        baseline,
    );

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

    let comparing = app.file().status != FileStatus::Current;
    let numbers_label = if comparing { " · old / new" } else { "" };
    let block = Block::bordered().title(format!(" {}{numbers_label} ", app.file().label));
    let inner = block.inner(source);
    frame.render_widget(block, source);
    let digits = app
        .file()
        .rows
        .iter()
        .flat_map(|row| [row.old_number, row.new_number])
        .flatten()
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let gutter_width = if comparing {
        digits * 2 + 4
    } else {
        digits + 2
    } as u16;
    let [gutter, code] =
        Layout::horizontal([Constraint::Length(gutter_width), Constraint::Min(0)]).areas(inner);
    app.set_viewport(code.height, code.width);
    for (offset, row) in app
        .file()
        .rows
        .iter()
        .skip(app.top)
        .take(app.height)
        .enumerate()
    {
        let number = |value: Option<usize>| value.map(|n| n.to_string()).unwrap_or_default();
        let numbers = if comparing {
            format!(
                "{:>digits$} {:>digits$} {} ",
                number(row.old_number),
                number(row.new_number),
                row.kind.marker()
            )
        } else {
            format!("{:>digits$}  ", number(row.new_number))
        };
        let gutter_row = Rect::new(gutter.x, gutter.y + offset as u16, gutter.width, 1);
        let code_row = Rect::new(code.x, code.y + offset as u16, code.width, 1);
        // Widget styles fill the whole row, including trailing blank columns.
        // Syntax spans only supply foreground colors, so they retain this tint.
        frame.render_widget(
            Paragraph::new(numbers).style(row.kind.style().fg(Color::Gray)),
            gutter_row,
        );
        frame.render_widget(
            Paragraph::new(row.text.clone())
                .style(row.kind.style())
                .scroll((0, app.left)),
            code_row,
        );
    }

    let position = if app.file().rows.is_empty() {
        "Empty file".into()
    } else {
        format!("Row {} / {}", app.top + 1, app.file().rows.len())
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(
                "↑/↓ j/k Scroll · ←/→ h/l Pan · PgUp/PgDn · Home/End · [/] Changes · Tab Files · ? Help · q Quit",
            ),
            Line::from(if app.decision_blocked.is_some() {
                format!("{position} · Conflicting package name: read-only · ? Help")
            } else if app.is_current_version_approved() {
                format!("{position} · Current version already approved · q Quit")
            } else if !app.can_decide() {
                format!("{position} · Fingerprint approval: read-only · q Quit")
            } else {
                format!("{position} · a Approve version · r Reject · {}", if app.queue_mode { "Decide and advance · n/p Packages · s Skip" } else { "Decide and exit" })
            }),
        ]),
        footer,
    );
    if app.show_help {
        render_help(frame, app);
    }
}

fn render_help(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let width = area.width.min(78);
    let height = area.height.min(if app.queue_mode { 20 } else { 18 });
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let mut lines = vec![
        Line::from("↑/↓ or j/k       Scroll source"),
        Line::from("←/→ or h/l       Pan long lines"),
        Line::from("PgUp / PgDn      Scroll a page"),
        Line::from("Home / End       First / last page (also g / G)"),
        Line::from("[ / ]            Previous / next change"),
        Line::from("Tab / Shift+Tab  Next / previous file"),
        Line::from(""),
    ];
    if app.queue_mode {
        lines.push(Line::from(
            "n / p            Next / previous package (including approved)",
        ));
        lines.push(Line::from(
            "s                Skip and advance to the next pending package",
        ));
    }
    if app.can_decide() {
        let after = if app.queue_mode { "advance" } else { "exit" };
        lines.push(Line::from(format!(
            "a  Save this package version's approval and {after}"
        )));
        lines.push(Line::from(format!(
            "r  Reject and {after}; keep existing approvals"
        )));
    } else if app.decision_blocked.is_some() {
        lines.push(Line::from(
            "Conflicting package names are read-only: one approval per name.",
        ));
    } else if app.is_current_version_approved() {
        lines.push(Line::from(
            "This version is already approved; viewing only.",
        ));
    } else {
        lines.push(Line::from(
            "Fingerprint approvals are read-only in version review.",
        ));
    }
    lines.extend([
        Line::from(""),
        Line::from("Approval covers every build script in this package version."),
        Line::from("This view shows entry-point files, not their dependencies."),
        Line::from(if app.queue_mode {
            "? / Esc  Close help     q / Ctrl+C  Exit; saved decisions are kept"
        } else {
            "? / Esc  Close help     q / Ctrl+C  Exit without changes"
        }),
    ]);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Review help "))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::review::app::tests::fixture;

    #[test]
    fn renders_queue_and_selected_package_at_different_sizes() {
        let (_root, metadata) = crate::review::tests::fixture();
        let queue = crate::review::discovery::ReviewQueue::from_metadata(&metadata).unwrap();
        let mut state = QueueState::new(queue);
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| render_queue(frame, &mut state))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Packages 1/4"));
        assert!(screen.contains("[conflict]"));
        assert!(screen.contains("custom-script v2.0.0"));
        assert!(screen.contains("compile_error!"));
        for (width, height) in [(80, 24), (40, 12), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_queue(frame, &mut state))
                .unwrap();
        }
    }

    #[test]
    fn renders_context_aware_help_and_fingerprint_read_only_controls() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        app.session.package.approval =
            Some(crate::config::ApprovedVersion::Fingerprint("hash".into()));
        let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Fingerprint approval: read-only"));
        assert!(!screen.contains("a Approve"));
        app.show_help = true;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Review help"));
        assert!(screen.contains("Fingerprint approvals are read-only"));
        assert!(!screen.contains("Save this package"));
        for (width, height) in [(40, 12), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }

    #[test]
    fn hides_decision_actions_for_approved_versions() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        app.session.package.approval = Some(crate::config::ApprovedVersion::SemVer(
            app.session.package.version.clone(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Current version already approved"));
        assert!(!screen.contains("a Approve"));
        assert!(!screen.contains("r Reject"));
    }

    #[test]
    fn colors_entire_diff_rows_while_retaining_syntax_foregrounds() {
        use crate::review::{
            diff::LineKind,
            sources::{PackageSources, ScriptSource},
        };

        let (_root, mut app) = fixture(&["fn main() {\n    let new = 2;\n}\n"]);
        app.set_baseline(Baseline::Ready(PackageSources {
            version: "0.0.1".parse().unwrap(),
            scripts: vec![ScriptSource {
                path: "script0.rs".into(),
                contents: "fn main() {\n    let old = 1;\n}\n".into(),
            }],
        }))
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        for kind in [LineKind::Addition, LineKind::Deletion] {
            let bg = kind.style().bg.unwrap();
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| cell.bg == bg && matches!(cell.fg, Color::Rgb(..)))
            );
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| cell.bg == bg && cell.symbol() == " ")
            );
        }
        for (width, height) in [(45, 15), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }

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
        assert!(screen.contains("Build script review"));
        app.switch_file(false);
        for (width, height) in [(100, 24), (40, 12), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }
}
