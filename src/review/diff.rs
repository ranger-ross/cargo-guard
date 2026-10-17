use std::{collections::BTreeMap, path::PathBuf};

use ratatui::{
    style::{Color, Style},
    text::Line,
};
use similar::{ChangeTag, TextDiff};

use super::{
    highlight::Highlighter,
    sources::{Baseline, ScriptSource},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
    Note,
}

impl LineKind {
    pub fn style(self) -> Style {
        match self {
            Self::Addition => Style::default().bg(Color::Rgb(22, 55, 36)),
            Self::Deletion => Style::default().bg(Color::Rgb(65, 28, 35)),
            _ => Style::default(),
        }
    }

    pub fn marker(self) -> &'static str {
        match self {
            Self::Addition => "+",
            Self::Deletion => "-",
            Self::Context => " ",
            Self::Note => "\\",
        }
    }
}

pub struct DiffLine {
    pub old_number: Option<usize>,
    pub new_number: Option<usize>,
    pub kind: LineKind,
    pub text: Line<'static>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Current,
    Unchanged,
    Added,
    Removed,
    Modified,
}

pub struct ReviewFile {
    pub path: PathBuf,
    pub label: String,
    pub status: FileStatus,
    pub rows: Vec<DiffLine>,
    pub width: usize,
    pub changes: Vec<usize>,
    pub additions: usize,
    pub deletions: usize,
    pub number_width: usize,
}

pub fn build_files(
    current: &[ScriptSource],
    baseline: &Baseline,
    highlighter: &Highlighter,
) -> eyre::Result<Vec<ReviewFile>> {
    let mut pairs = BTreeMap::new();
    for source in current {
        pairs.entry(source.path.clone()).or_insert((None, None)).1 = Some(source.contents.as_str());
    }
    if let Baseline::Ready(previous) = baseline {
        for source in &previous.scripts {
            pairs.entry(source.path.clone()).or_insert((None, None)).0 =
                Some(source.contents.as_str());
        }
    }
    let comparing = matches!(baseline, Baseline::Ready(_) | Baseline::NewPackage);
    pairs
        .into_iter()
        .map(|(path, (old, new))| build_file(path, old, new, comparing, highlighter))
        .collect()
}

fn build_file(
    path: PathBuf,
    old: Option<&str>,
    new: Option<&str>,
    comparing: bool,
    highlighter: &Highlighter,
) -> eyre::Result<ReviewFile> {
    let status = if !comparing {
        FileStatus::Current
    } else {
        match (old, new) {
            (None, _) => FileStatus::Added,
            (_, None) => FileStatus::Removed,
            (Some(old), Some(new)) if old == new => FileStatus::Unchanged,
            _ => FileStatus::Modified,
        }
    };
    // Highlight each complete revision independently, then select the tokens
    // belonging to each diff row. Deletions retain the OLD file's syntax state.
    let old_lines = highlighter.highlight(old.unwrap_or(""))?;
    let new_lines = highlighter.highlight(new.unwrap_or(""))?;
    let mut rows = Vec::new();
    if !comparing {
        rows.extend(
            new_lines
                .into_iter()
                .enumerate()
                .map(|(index, text)| DiffLine {
                    old_number: None,
                    new_number: Some(index + 1),
                    kind: LineKind::Context,
                    text,
                }),
        );
    } else {
        let diff = TextDiff::from_lines(old.unwrap_or(""), new.unwrap_or(""));
        for change in diff.iter_all_changes() {
            let kind = match change.tag() {
                ChangeTag::Equal => LineKind::Context,
                ChangeTag::Insert => LineKind::Addition,
                ChangeTag::Delete => LineKind::Deletion,
            };
            let text = if kind == LineKind::Deletion {
                old_lines[change.old_index().expect("deletion has an old index")].clone()
            } else {
                new_lines[change
                    .new_index()
                    .expect("addition/context has a new index")]
                .clone()
            };
            rows.push(DiffLine {
                old_number: change.old_index().map(|index| index + 1),
                new_number: change.new_index().map(|index| index + 1),
                kind,
                text,
            });
            if change.missing_newline() {
                rows.push(DiffLine {
                    old_number: None,
                    new_number: None,
                    kind: LineKind::Note,
                    text: Line::styled(
                        "No newline at end of file",
                        Style::default().fg(Color::DarkGray),
                    ),
                });
            }
        }
    }
    let added = rows
        .iter()
        .filter(|row| row.kind == LineKind::Addition)
        .count();
    let removed = rows
        .iter()
        .filter(|row| row.kind == LineKind::Deletion)
        .count();
    let description = match status {
        FileStatus::Current => "current source".into(),
        FileStatus::Unchanged => "unchanged".into(),
        FileStatus::Added => format!("added +{added}"),
        FileStatus::Removed => format!("removed -{removed}"),
        FileStatus::Modified => format!("+{added} -{removed}"),
    };
    let mut changes = Vec::new();
    let mut in_change = false;
    for (index, row) in rows.iter().enumerate() {
        match row.kind {
            LineKind::Addition | LineKind::Deletion => {
                if !in_change {
                    changes.push(index);
                }
                in_change = true;
            }
            LineKind::Context => in_change = false,
            LineKind::Note => {}
        }
    }
    Ok(ReviewFile {
        label: format!("{} [{description}]", path.display()),
        additions: added,
        deletions: removed,
        number_width: rows
            .iter()
            .flat_map(|row| [row.old_number, row.new_number])
            .flatten()
            .max()
            .unwrap_or(1)
            .to_string()
            .len(),
        path,
        status,
        width: rows.iter().map(|row| row.text.width()).max().unwrap_or(0),
        rows,
        changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::sources::PackageSources;

    fn source(path: &str, text: &str) -> ScriptSource {
        ScriptSource {
            path: path.into(),
            contents: text.into(),
        }
    }

    #[test]
    fn tracks_line_numbers_changes_and_missing_newlines() {
        let file = build_file(
            "build.rs".into(),
            Some("a\nb\nc\nd\n"),
            Some("a\nB\nc\nD"),
            true,
            &Highlighter::new().unwrap(),
        )
        .unwrap();
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!(file.changes, [1, 4]);
        assert_eq!(file.rows[1].old_number, Some(2));
        assert_eq!(file.rows[1].new_number, None);
        assert_eq!(file.rows[1].kind, LineKind::Deletion);
        assert_eq!(file.rows[2].old_number, None);
        assert_eq!(file.rows[2].new_number, Some(2));
        assert_eq!(file.rows[2].kind, LineKind::Addition);
        assert_eq!(file.rows.last().unwrap().kind, LineKind::Note);
        assert!(
            file.rows[2]
                .text
                .spans
                .iter()
                .all(|span| span.style.bg.is_none())
        );
    }

    #[test]
    fn pairs_by_path_and_retains_added_removed_and_empty_scripts() {
        let current = [
            source("renamed.rs", "fn main() {}\n"),
            source("same.rs", "same\n"),
            source("empty.rs", ""),
        ];
        let baseline = Baseline::Ready(PackageSources {
            version: "1.0.0".parse().unwrap(),
            scripts: vec![
                source("old.rs", "fn main() {}\n"),
                source("same.rs", "same\n"),
            ],
        });
        let files = build_files(&current, &baseline, &Highlighter::new().unwrap()).unwrap();
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].status, FileStatus::Added);
        assert!(files[0].rows.is_empty());
        assert_eq!(files[1].status, FileStatus::Removed);
        assert_eq!(files[2].status, FileStatus::Added);
        assert_eq!(files[3].status, FileStatus::Unchanged);
        assert!(files[3].changes.is_empty());
    }

    #[test]
    fn unavailable_baseline_is_not_treated_as_an_added_file() {
        let current = [source("build.rs", "fn main() {}\n")];
        let highlighter = Highlighter::new().unwrap();
        let added = build_files(&current, &Baseline::NewPackage, &highlighter).unwrap();
        assert_eq!(added[0].rows[0].kind, LineKind::Addition);
        let unavailable = Baseline::Unavailable {
            version: "1.0.0".parse().unwrap(),
            reason: "offline".into(),
        };
        let files = build_files(&current, &unavailable, &highlighter).unwrap();
        assert_eq!(files[0].status, FileStatus::Current);
        assert_eq!(files[0].rows[0].kind, LineKind::Context);
    }
}
