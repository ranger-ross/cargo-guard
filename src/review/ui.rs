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
    diagnostic::Diagnostic,
    diff::FileStatus,
    queue::QueueState,
    sources::Baseline,
};
use crate::config::{ApprovalScope, ApprovalTarget, ApprovedVersion};

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
    let block = Block::new()
        .borders(if wide {
            Borders::RIGHT
        } else {
            Borders::BOTTOM
        })
        .border_style(muted())
        .title(Line::styled(
            format!("Packages {}/{}", state.selected + 1, state.entries.len()),
            muted(),
        ));
    let rows = block.inner(packages);
    let visible = usize::from(rows.height).max(1);
    let start = state
        .selected
        .saturating_sub(visible / 2)
        .min(state.entries.len().saturating_sub(visible));
    state.package_rows = Rect {
        height: usize::from(rows.height).min(state.entries.len() - start) as u16,
        ..rows
    };
    state.package_start = start;
    let hovered = state.hovered_package();
    let items: Vec<_> = state
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, entry)| {
            let (marker, color) = match entry.status() {
                "approved" | "already approved" | "approved globally" => ("✓", Color::Green),
                "rejected" => ("×", Color::Red),
                "load error" | "baseline unavailable" => ("!", Color::Yellow),
                "skipped" => ("–", Color::DarkGray),
                _ => ("·", Color::DarkGray),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(color)),
                Span::raw(format!("{} v{}", entry.package.name, entry.package.version)),
            ]))
            .style(if hovered == Some(index) {
                Style::default().fg(Color::White).bg(Color::Rgb(35, 40, 48))
            } else {
                Style::default()
            })
        })
        .collect();
    let list = List::new(items)
        .block(block)
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
        render_failure_area(frame, state, review);
    }
}

pub fn render_failure(frame: &mut Frame, state: &mut QueueState) {
    render_failure_area(frame, state, frame.area());
}

fn render_failure_area(frame: &mut Frame, state: &mut QueueState, area: Rect) {
    let entry = &state.entries[state.selected];
    let report = entry
        .error
        .clone()
        .unwrap_or_else(|| "Could not load build script sources".into());
    state.error_diagnostic.set_report(report, true);
    let [header, error, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(status_line(
            "Reviewing",
            format!("{} v{}", entry.package.name, entry.package.version),
            Color::Green,
        )),
        header,
    );
    state.error_diagnostic.render(frame, error);
    frame.render_widget(
        Paragraph::new("? help  y copy  Esc clear  q quit").style(muted()),
        footer,
    );
    if state.show_failure_help {
        let mut text = approval_destination(&state.approval_target);
        for note in &state.policy_notes {
            text.push_str("\n\n");
            text.push_str(note);
        }
        text.push_str(
            "\n\nSources could not be loaded; approval actions are unavailable.\n\
             j / k  Next / previous package\n\
             s      Skip to the next pending package\n\
             y / Esc  Copy diagnostic selection / clear selection\n\
             q / Ctrl+C  Quit; saved decisions are kept",
        );
        state.failure_help.set_report(text, false);
        render_help_popup(frame, &mut state.failure_help);
    }
}

fn update_diagnostic(app: &mut App) {
    let error = app.notice.as_ref().is_some_and(|notice| notice.is_error);
    let message = if error {
        app.notice.as_ref().map(|notice| notice.message.clone())
    } else if let Some(reason) = &app.decision_blocked {
        Some(reason.clone())
    } else if let Baseline::Unavailable { version, reason } = &app.baseline {
        Some(format!(
            "Could not load approved v{version} sources.\n\n{reason}"
        ))
    } else {
        None
    };
    app.diagnostic
        .set_report(message.unwrap_or_default(), error);
}

fn render_area(frame: &mut Frame, app: &mut App, area: Rect) {
    update_diagnostic(app);
    let note = baseline_note(app);
    let notice = app
        .notice
        .as_ref()
        .filter(|notice| !notice.is_error)
        .map(|notice| status_line("Finished", notice.message.clone(), Color::Green));
    let multiple = app.files.len() > 1;
    let diagnostic_height = app.diagnostic.height(area.width, area.height);
    let [
        header,
        notice_area,
        diagnostic,
        baseline,
        tabs,
        source,
        footer,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(message_height(notice.as_ref(), area.width)),
        Constraint::Length(diagnostic_height),
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
    app.diagnostic.render(frame, diagnostic);
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
    app.selection.area = code;
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
        if row.kind != super::diff::LineKind::Note
            && let Some(columns) = app.selection.columns(app.top + offset)
        {
            let left = usize::from(app.left);
            let start = columns
                .start
                .saturating_sub(left)
                .min(usize::from(code.width));
            let end = columns
                .end
                .min(row.text.width() + 1)
                .saturating_sub(left)
                .min(usize::from(code.width));
            for column in start..end {
                frame.buffer_mut()[(code.x + column as u16, code_row.y)]
                    .set_style(Style::default().fg(Color::Black).bg(Color::LightBlue));
            }
        }
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
        keys.push(("a", "approve"));
        keys.push(("r", "reject"));
    }
    if app.queue_mode {
        keys.push(("s", "skip"));
    }
    keys.push(("o", "open"));
    if app.selection.is_active() || app.diagnostic.selection.is_active() {
        keys.push(("y", "copy"));
        keys.push(("Esc", "clear"));
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

fn render_help(frame: &mut Frame, app: &mut App) {
    let mut lines = vec![
        Line::from("Up / Down        Scroll source"),
        Line::from("←/→ or h/l       Pan long lines"),
        Line::from("PgUp / PgDn      Scroll a page"),
        Line::from("Home / End       First / last page (also g / G)"),
        Line::from("[ / ]            Previous / next change"),
        Line::from("Tab / Shift+Tab  Next / previous file"),
        Line::from("o                Open selected script in the browser"),
        Line::from("y / Esc          Copy selection / clear selection"),
        Line::from("Drag / y         Select and copy source or diagnostic text"),
    ];
    if app.queue_mode {
        lines.push(Line::from(
            "j / k            Next / previous package (including approved)",
        ));
        lines.push(Line::from(
            "s                Skip to the next pending package",
        ));
        lines.push(Line::styled(
            "✓ approved  × rejected  ! unresolved  – skipped",
            muted(),
        ));
    }
    lines.push(Line::default());
    if app.can_decide() {
        let after = if app.queue_mode { "advance" } else { "exit" };
        lines.push(Line::from(format!(
            "a  Approve v{} and {after}",
            app.session.package.version
        )));
        lines.push(Line::from(format!(
            "r  Reject and {after}; keep existing approvals"
        )));
    } else if let Some(reason) = &app.decision_blocked {
        lines.push(Line::from(reason.as_str()));
    } else if app.is_current_version_approved() {
        lines.push(Line::from(
            "This version is already approved; viewing only.",
        ));
    }
    if app.globally_approved {
        lines.push(Line::from(
            "Global approval is informational here; workspace review still saves only workspace approvals.",
        ));
        lines.push(Line::from(
            "Build precedence is unchanged: an existing workspace approval file remains authoritative for builds in that workspace.",
        ));
    }
    lines.extend([
        Line::default(),
        Line::from("Diff columns: approved / current. Approval covers all scripts in the package."),
        Line::from(
            "Approvals apply by package name across sources; this view shows entry points only.",
        ),
        Line::from(
            "Browser links use the current version, or the approved version for removed files.",
        ),
        Line::from("Remote source is not verified against the local files being reviewed."),
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
    ]);
    let mut text = approval_destination(app.session.config.target());
    for note in &app.session.policy_notes {
        text.push_str("\n\n");
        text.push_str(note);
    }
    text.push('\n');
    for line in lines {
        text.push('\n');
        for span in line.spans {
            text.push_str(&span.content);
        }
    }
    app.help.set_report(text, false);
    render_help_popup(frame, &mut app.help);
}

fn render_help_popup(frame: &mut Frame, help: &mut Diagnostic) {
    let area = frame.area();
    let width = area.width.min(90);
    let height = area.height.min(34);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::bordered()
        .border_style(muted())
        .title(Span::styled(" Review help ", bold(Color::Green)));
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(block.inner(popup));
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    help.render_body(frame, body);
    frame.render_widget(
        Paragraph::new("↑↓ PgUp/Dn Home/End scroll · ?/Esc close").style(muted()),
        footer,
    );
}

fn approval_destination(target: &ApprovalTarget) -> String {
    format!("Approvals ({}): {}", target.scope, target.path.display())
}

fn heading(app: &App) -> Line<'static> {
    let package = &app.session.package;
    let (label, color) = if app.decision_blocked.is_some() {
        ("Blocked", Color::Yellow)
    } else if app.decision == Some(Decision::Reject) {
        ("Rejected", Color::Red)
    } else if let Some(scope) = app.displayed_approval_scope() {
        let label = if scope == ApprovalScope::Global {
            "Approved (global)"
        } else {
            "Approved"
        };
        (label, Color::Green)
    } else {
        ("Reviewing", Color::Green)
    };
    let supplemental_only =
        app.globally_approved && !app.is_blanket_approved() && !app.is_current_version_approved();
    let baseline_version = match &package.approval {
        Some(ApprovedVersion::SemVer(version)) if !supplemental_only => Some(version),
        _ => None,
    };
    let version = match baseline_version {
        Some(old) if old != &package.version => {
            format!("{} v{old} -> v{}", package.name, package.version)
        }
        _ => format!("{} v{}", package.name, package.version),
    };
    let mut line = status_line(label, version, color);
    let suffix = if supplemental_only {
        ""
    } else {
        match &package.approval {
            None => " (new)",
            Some(ApprovedVersion::Blanket(_)) => " (all versions)",
            _ => "",
        }
    };
    line.spans.push(Span::styled(suffix, muted()));
    line
}

fn baseline_note(app: &App) -> Option<Line<'static>> {
    if app.decision_blocked.is_some() {
        return None;
    }
    match &app.baseline {
        Baseline::Loading(version) => Some(status_line(
            "Fetching",
            format!("approved v{version}"),
            Color::Green,
        )),
        Baseline::Unavailable { .. } => None,
        Baseline::Ready(_)
            if app
                .files
                .iter()
                .all(|file| file.status == FileStatus::Unchanged) =>
        {
            Some(status_line(
                "Unchanged",
                "build script entry points match the approved version".into(),
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
        let queue = crate::review::discovery::ReviewQueue::from_metadata(
            &metadata,
            &crate::args::ReviewOptions::default(),
            None,
        )
        .unwrap();
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
    fn narrow_help_scrolls_without_changing_source_or_package_selection() {
        use crate::review::input::{self, Action};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        app.queue_mode = true;
        app.session.policy_notes = vec!["policy-note ".repeat(40) + "note-end-marker"];
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        input::handle_key(&mut app, key(KeyCode::Char('?')));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let first = screen(&terminal);
        let mut saw_note_end = false;
        for _ in 0..80 {
            assert_eq!(
                input::handle_key(&mut app, key(KeyCode::Char('j'))),
                Action::Continue,
            );
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            saw_note_end |= screen(&terminal).contains("note-end-marker");
        }
        assert!(saw_note_end);
        assert_ne!(screen(&terminal), first);
        assert_eq!(app.top, 0);
        assert_eq!(app.selected, 0);
        input::handle_key(&mut app, key(KeyCode::Home));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(screen(&terminal), first);
        input::handle_key(&mut app, key(KeyCode::Esc));
        assert!(!app.show_help);
    }

    #[test]
    fn failure_help_scrolls_and_blocks_package_actions() {
        use crate::review::input::Action;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (_root, metadata) = crate::review::tests::fixture();
        let queue = crate::review::discovery::ReviewQueue::from_metadata(
            &metadata,
            &crate::args::ReviewOptions::default(),
            None,
        )
        .unwrap();
        let mut state = QueueState::new(queue);
        state.app = None;
        state.policy_notes = vec!["policy-note ".repeat(40) + "note-end-marker"];
        let selected = state.selected;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        state.handle_key(key(KeyCode::Char('?')));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| render_failure(frame, &mut state))
            .unwrap();
        let first = screen(&terminal);
        assert_eq!(state.handle_key(key(KeyCode::Char('s'))), Action::Continue);
        state.handle_key(key(KeyCode::End));
        terminal
            .draw(|frame| render_failure(frame, &mut state))
            .unwrap();
        assert_ne!(screen(&terminal), first);
        assert_eq!(state.selected, selected);
        state.handle_key(key(KeyCode::Home));
        terminal
            .draw(|frame| render_failure(frame, &mut state))
            .unwrap();
        assert_eq!(screen(&terminal), first);
        assert_eq!(state.handle_key(key(KeyCode::Esc)), Action::Continue);
        assert!(!state.show_failure_help);
    }

    #[test]
    fn colors_entire_diff_rows_while_retaining_syntax_foregrounds() {
        use crate::review::sources::{PackageSources, ScriptSource};
        let (_root, mut app) = fixture(&["fn main() {\n    let new = 2;\n}\n"]);
        app.set_baseline(Baseline::Ready(PackageSources {
            version: "0.0.1".parse().unwrap(),
            browser_source: Default::default(),
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
    fn blanket_approval_displays_its_scope_without_version_checks() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        app.session.package.approval = Some(ApprovedVersion::Blanket(
            crate::config::BlanketApproval::Approved,
        ));
        app.set_baseline(Baseline::for_session(&app.session))
            .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Approved example v0.1.0 (all versions)"));
        assert!(!text.contains("Fetching"));
        assert!(!text.contains("[a]"));
        assert!(!text.contains("[r]"));
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
