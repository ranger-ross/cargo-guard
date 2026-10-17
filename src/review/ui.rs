use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use unicode_width::UnicodeWidthStr;

use super::{
    app::{App, Decision},
    diff::FileStatus,
    queue::QueueState,
    sources::Baseline,
};
use crate::config::ApprovedVersion;

pub fn render(frame: &mut Frame, app: &mut App) {
    render_area(frame, app, frame.area());
}

pub fn render_queue(frame: &mut Frame, state: &mut QueueState) {
    let wide = frame.area().width >= 100;
    let sidebar_width = state.sidebar_width;
    let [packages, review] = if wide {
        Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(0)])
            .areas(frame.area())
    } else {
        Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).areas(frame.area())
    };
    let [packages, package_help] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(packages);
    let visible = usize::from(packages.height.saturating_sub(if wide { 1 } else { 2 })).max(1);
    let start = state
        .selected
        .saturating_sub(visible / 2)
        .min(state.entries.len().saturating_sub(visible));
    let items: Vec<_> = state
        .entries
        .iter()
        .skip(start)
        .take(visible)
        .map(|entry| {
            let (marker, color) = match entry.status() {
                "approved" | "already approved" => ("✓", Color::Green),
                "rejected" => ("×", Color::Red),
                "load error" | "baseline unavailable" => ("!", Color::Yellow),
                "fingerprint" => ("?", Color::Yellow),
                "skipped" => ("–", Color::DarkGray),
                _ => ("·", Color::DarkGray),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(color)),
                Span::raw(format!("{} v{}", entry.package.name, entry.package.version)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::new()
                .borders(if wide {
                    Borders::RIGHT
                } else {
                    Borders::BOTTOM
                })
                .border_style(muted())
                .title(Line::styled(
                    format!("Packages {}/{}", state.selected + 1, state.entries.len()),
                    muted(),
                )),
        )
        .highlight_symbol("> ")
        .highlight_style(bold(Color::Green));
    frame.render_stateful_widget(
        list,
        packages,
        &mut ListState::default().with_selected(Some(state.selected - start)),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("j/k", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(" scroll packages", muted()),
        ]))
        .block(
            Block::new()
                .borders(if wide { Borders::RIGHT } else { Borders::NONE })
                .border_style(muted()),
        ),
        package_help,
    );
    if let Some(app) = &mut state.app {
        render_area(frame, app, review);
    } else {
        let entry = &state.entries[state.selected];
        frame.render_widget(
            Paragraph::new(vec![
                status_line(
                    "Reviewing",
                    format!("{} v{}", entry.package.name, entry.package.version),
                    Color::Green,
                ),
                Line::default(),
                status_line(
                    "error",
                    entry
                        .error
                        .as_deref()
                        .unwrap_or("could not load sources")
                        .to_owned(),
                    Color::Red,
                ),
                Line::default(),
                Line::styled("s skip   q quit", muted()),
            ])
            .wrap(Wrap { trim: false }),
            review,
        );
    }
}

fn render_area(frame: &mut Frame, app: &mut App, area: Rect) {
    let note = baseline_note(app);
    let notice = app.notice.as_ref().map(|notice| {
        status_line(
            if notice.is_error { "error" } else { "Finished" },
            notice.message.clone(),
            if notice.is_error {
                Color::Red
            } else {
                Color::Green
            },
        )
    });
    let multiple = app.files.len() > 1;
    let [header, notice_area, baseline, tabs, source, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(message_height(notice.as_ref(), area.width)),
        Constraint::Length(message_height(note.as_ref(), area.width)),
        Constraint::Length(if multiple { 2 } else { 0 }),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(notice).wrap(Wrap { trim: false }),
            notice_area,
        );
    }
    if let Some(note) = note {
        frame.render_widget(Paragraph::new(note).wrap(Wrap { trim: false }), baseline);
    }
    if multiple {
        let labels: Vec<_> = app
            .files
            .iter()
            .map(|file| file.path.display().to_string())
            .collect();
        frame.render_widget(
            Tabs::new(labels)
                .select(app.selected)
                .style(muted())
                .highlight_style(bold(Color::Green).add_modifier(Modifier::UNDERLINED)),
            tabs,
        );
    }

    let file = app.file();
    let added = file.additions;
    let removed = file.deletions;
    let mut label = Line::from(vec![Span::styled(
        format!("  {}", file.path.display()),
        Style::default().add_modifier(Modifier::BOLD),
    )]);
    if added > 0 {
        label.spans.push(Span::styled(
            format!("  +{added}"),
            Style::default().fg(Color::Green),
        ));
    }
    if removed > 0 {
        label.spans.push(Span::styled(
            format!("  -{removed}"),
            Style::default().fg(Color::Red),
        ));
    }
    if file.rows.is_empty() {
        label.spans.push(Span::styled(
            match file.status {
                FileStatus::Added => "  (new empty file)",
                FileStatus::Removed => "  (deleted empty file)",
                _ => "  (empty)",
            },
            muted(),
        ));
    }
    let [package_header, file_header] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(label.width().min(usize::from(header.width)) as u16),
    ])
    .areas(header);
    frame.render_widget(Paragraph::new(heading(app)), package_header);
    frame.render_widget(Paragraph::new(label), file_header);

    let comparing = app.file().status != FileStatus::Current;
    let digits = app.file().number_width;
    let gutter_width = if comparing {
        digits * 2 + 4
    } else {
        digits + 2
    } as u16;
    let [gutter, code] =
        Layout::horizontal([Constraint::Length(gutter_width), Constraint::Min(0)]).areas(source);
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
        frame.render_widget(
            Paragraph::new(numbers).style(row.kind.style().fg(Color::DarkGray)),
            gutter_row,
        );
        frame.render_widget(
            Paragraph::new(
                Line::from(
                    row.text
                        .spans
                        .iter()
                        .map(|span| Span::styled(span.content.as_ref(), span.style))
                        .collect::<Vec<_>>(),
                )
                .style(row.text.style),
            )
            .style(row.kind.style())
            .scroll((0, app.left)),
            code_row,
        );
    }
    render_footer(frame, app, footer);
    if app.show_help {
        render_help(frame, app);
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let [actions, help] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(16)]).areas(area);
    let mut keys = Vec::new();
    if app.can_decide() {
        keys.push((
            "a",
            if app.prefers_fingerprint() {
                "fingerprints"
            } else {
                "approve"
            },
        ));
        if !app.prefers_fingerprint() {
            keys.push(("f", "fingerprints"));
        }
        keys.push(("r", "reject"));
    }
    if app.queue_mode {
        keys.push(("s", "skip"));
    }
    keys.push(("↑/↓/←/→", "scroll"));
    if app.files.len() > 1 {
        keys.push(("Tab", "files"));
    }
    let spans: Vec<_> = keys
        .into_iter()
        .flat_map(|(key, action)| {
            [
                Span::styled(key, Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {action}   "), muted()),
            ]
        })
        .collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), actions);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("?"),
            Span::styled(" help  ", muted()),
            Span::raw("q"),
            Span::styled(" quit", muted()),
        ])),
        help,
    );
}

fn render_help(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let width = area.width.min(90);
    let height = area.height.min(if app.queue_mode { 30 } else { 27 });
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let mut lines = vec![
        Line::from("Up / Down        Scroll source"),
        Line::from("←/→ or h/l       Pan long lines"),
        Line::from("PgUp / PgDn      Scroll a page"),
        Line::from("Home / End       First / last page (also g / G)"),
        Line::from("[ / ]            Previous / next change"),
        Line::from("Tab / Shift+Tab  Next / previous file"),
    ];
    if app.queue_mode {
        lines.push(Line::from(
            "j / k            Next / previous package (including approved)",
        ));
        lines.push(Line::from(
            "s                Skip to the next pending package",
        ));
        lines.push(Line::styled(
            "✓ approved  × rejected  ! unresolved  ? fingerprint  – skipped",
            muted(),
        ));
    }
    lines.push(Line::default());
    if app.can_decide() {
        let after = if app.queue_mode { "advance" } else { "exit" };
        lines.push(Line::from(format!(
            "a  Approve {} and {after}",
            if app.prefers_fingerprint() {
                "current fingerprints"
            } else {
                "this package version"
            }
        )));
        lines.push(Line::from(
            "f  Approve all script fingerprints using the latest compiled inputs",
        ));
        lines.push(Line::from(format!(
            "r  Reject and {after}; keep existing approvals"
        )));
    } else if let Some(reason) = &app.decision_blocked {
        lines.push(Line::from(reason.as_str()));
    } else if app.is_current_version_approved() {
        lines.push(Line::from(
            "This version is already approved; viewing only.",
        ));
    } else {
        lines.push(Line::from("Current fingerprints have been approved."));
    }
    lines.extend([
        Line::default(),
        Line::from("Diff columns: approved / current. Approval covers all scripts in the package."),
        Line::from(
            "Approvals apply by package name across sources; this view shows entry points only.",
        ),
        Line::from("? / Esc close help · q / Ctrl+C quit; saved decisions are kept"),
        Line::default(),
        Line::styled(app.approval_status(), muted()),
        Line::styled(app.baseline_message(), muted()),
        Line::styled(format!("File: {}", app.file().label), muted()),
        Line::styled(format!("Package: {}", app.session.package.id), muted()),
        Line::styled(
            format!(
                "Source: {}",
                app.session
                    .package
                    .source
                    .as_ref()
                    .map_or("local path", |source| source.repr.as_str())
            ),
            muted(),
        ),
        Line::styled(
            format!("Approvals: {}", app.session.config.path().display()),
            muted(),
        ),
    ]);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_style(muted())
                    .title(Span::styled(" Review help ", bold(Color::Green))),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn heading(app: &App) -> Line<'static> {
    let package = &app.session.package;
    let (label, color) = if app.decision_blocked.is_some() {
        ("Blocked", Color::Yellow)
    } else if app.decision == Some(Decision::Reject) {
        ("Rejected", Color::Red)
    } else if app.is_current_version_approved()
        || app.decision == Some(Decision::ApproveFingerprint)
    {
        ("Approved", Color::Green)
    } else {
        ("Reviewing", Color::Green)
    };
    let version = match &package.approval {
        Some(ApprovedVersion::SemVer(old)) if old != &package.version => {
            format!("{} v{old} -> v{}", package.name, package.version)
        }
        _ => format!("{} v{}", package.name, package.version),
    };
    let mut line = status_line(label, version, color);
    let suffix = match &package.approval {
        None => " (new)",
        Some(ApprovedVersion::Fingerprint(_)) => " (fingerprint)",
        _ => "",
    };
    line.spans.push(Span::styled(suffix, muted()));
    line
}

fn baseline_note(app: &App) -> Option<Line<'static>> {
    if let Some(reason) = &app.decision_blocked {
        return Some(status_line("warning", reason.clone(), Color::Yellow));
    }
    match &app.baseline {
        Baseline::Loading(version) => Some(status_line(
            "Fetching",
            format!("approved v{version}"),
            Color::Green,
        )),
        Baseline::Unavailable { version, reason } => Some(status_line(
            "warning",
            format!("approved v{version} source unavailable: {reason}"),
            Color::Yellow,
        )),
        Baseline::Ready(_)
            if app
                .files
                .iter()
                .all(|file| file.status == FileStatus::Unchanged) =>
        {
            Some(status_line(
                "Unchanged",
                "build scripts; the new version still needs approval".into(),
                Color::Green,
            ))
        }
        _ => None,
    }
}

fn message_height(line: Option<&Line<'_>>, width: u16) -> u16 {
    line.map_or(0, |line| {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        text.lines()
            .map(|line| line.width().div_ceil(usize::from(width).max(1)).max(1))
            .sum::<usize>()
            .clamp(1, 4) as u16
    })
}

fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

// Cargo uses right-aligned, bold status verbs and the terminal's own palette.
fn status_line(label: &str, message: String, color: Color) -> Line<'static> {
    let label = if matches!(label, "error" | "warning") {
        format!("{label}: ")
    } else {
        format!("{label:>12} ")
    };
    Line::from(vec![Span::styled(label, bold(color)), Span::raw(message)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::app::tests::fixture;
    use crate::review::diff::LineKind;
    use ratatui::{Terminal, backend::TestBackend};

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_queue_and_selected_package_at_different_sizes() {
        let (_root, metadata) = crate::review::tests::fixture();
        let queue = crate::review::discovery::ReviewQueue::from_metadata(&metadata).unwrap();
        let mut state = QueueState::new(queue);
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal
            .draw(|frame| render_queue(frame, &mut state))
            .unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Packages 1/4"));
        assert!(text.contains("· duplicate"));
        assert!(text.contains("custom-script v2.0.0"));
        assert!(text.contains("compile_error!"));
        for (width, height) in [(80, 24), (40, 12), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_queue(frame, &mut state))
                .unwrap();
        }
    }

    #[test]
    fn renders_context_aware_help_and_fingerprint_controls() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        app.session.package.approval = Some(ApprovedVersion::Fingerprint("hash".into()));
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("(fingerprint)"));
        assert!(text.contains("a fingerprints"));
        assert!(!text.contains("a approve"));
        assert!(!text.contains("Approvals:"));
        app.show_help = true;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Review help"));
        assert!(text.contains("Approve current fingerprints"));
        assert!(text.contains("Approvals:"));
        for (width, height) in [(40, 12), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }

    #[test]
    fn hides_decision_actions_for_approved_versions() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        app.session.package.approval =
            Some(ApprovedVersion::SemVer(app.session.package.version.clone()));
        let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Approved example v0.1.0"));
        assert!(!text.contains("a approve"));
        assert!(!text.contains("r reject"));
    }

    #[test]
    fn colors_entire_diff_rows_while_retaining_syntax_foregrounds() {
        use crate::review::sources::{PackageSources, ScriptSource};
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
    fn keeps_the_main_view_minimal_and_uses_cargo_status_colors() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Reviewing example v0.1.0 (new)"));
        assert!(text.contains("fn main()"));
        for removed in [
            "Source:",
            "Approvals:",
            "Baseline:",
            "Files (",
            "Build script review",
            "┌",
            "┐",
        ] {
            assert!(!text.contains(removed), "unexpected chrome: {removed}");
        }
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.symbol() == "R"
                    && cell.fg == Color::Green
                    && cell.modifier.contains(Modifier::BOLD))
        );
        app.session.package.approval = Some(ApprovedVersion::SemVer(semver::Version::new(0, 0, 1)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("v0.0.1 -> v0.1.0"));
    }

    #[test]
    fn renders_source_status_and_empty_files_at_small_sizes() {
        let (_root, mut app) = fixture(&["fn main() {\n    println!(\"hello\");\n}", ""]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("fn main()"));
        assert!(screen(&terminal).contains("script1.rs"));
        app.switch_file(false);
        for (width, height) in [(100, 24), (40, 12), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }
}
