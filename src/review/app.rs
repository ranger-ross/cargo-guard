use std::fs;

use super::{
    ReviewSession,
    diff::{self, FileStatus, ReviewFile},
    highlight::Highlighter,
    sources::{Baseline, ScriptSource},
};
use crate::config::ApprovedVersion;
use eyre::Context;

pub struct App {
    pub session: ReviewSession,
    pub baseline: Baseline,
    pub files: Vec<ReviewFile>,
    current: Vec<ScriptSource>,
    highlighter: Highlighter,
    pub selected: usize,
    pub top: usize,
    pub left: u16,
    pub height: usize,
    pub width: usize,
    change_target: Option<usize>,
}

impl App {
    pub fn new(session: ReviewSession) -> eyre::Result<Self> {
        let highlighter = Highlighter::new()?;
        let root = session.package.manifest_path.parent();
        let current = session
            .package
            .scripts
            .iter()
            .map(|script| {
                let contents = fs::read_to_string(&script.path).with_context(|| {
                    format!("could not read build script {}", script.path.display())
                })?;
                let path = root
                    .and_then(|root| script.path.strip_prefix(root).ok())
                    .unwrap_or(&script.path);
                Ok(ScriptSource {
                    path: path.to_path_buf(),
                    contents,
                })
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        eyre::ensure!(!current.is_empty(), "no build scripts to display");
        let baseline = Baseline::for_session(&session);
        let files = diff::build_files(&current, &baseline, &highlighter)?;
        Ok(Self {
            baseline,
            session,
            files,
            current,
            highlighter,
            selected: 0,
            top: 0,
            left: 0,
            height: 0,
            width: 0,
            change_target: None,
        })
    }

    pub fn file(&self) -> &ReviewFile {
        &self.files[self.selected]
    }

    pub fn set_baseline(&mut self, baseline: Baseline) -> eyre::Result<()> {
        let files = diff::build_files(&self.current, &baseline, &self.highlighter)?;
        self.selected = files
            .iter()
            .position(|file| file.path == self.file().path)
            .unwrap_or(0);
        self.files = files;
        self.baseline = baseline;
        self.top = 0;
        self.left = 0;
        self.change_target = None;
        Ok(())
    }

    pub fn baseline_message(&self) -> String {
        let mut message = self.baseline.message();
        if matches!(self.baseline, Baseline::Ready(_))
            && self
                .files
                .iter()
                .all(|file| file.status == FileStatus::Unchanged)
        {
            message.push_str(" · Build scripts unchanged");
        }
        message
    }

    pub fn jump_change(&mut self, backwards: bool) {
        let changes = &self.file().changes;
        let position = self.change_target.unwrap_or(self.top);
        let next = if backwards {
            changes
                .iter()
                .rev()
                .find(|&&row| row < position)
                .or_else(|| changes.last())
        } else {
            changes
                .iter()
                .find(|&&row| row > position)
                .or_else(|| changes.first())
        };
        if let Some(&row) = next {
            self.change_target = Some(row);
            self.top = row.min(self.max_top());
        }
    }

    pub fn approval_status(&self) -> String {
        match &self.session.package.approval {
            Some(ApprovedVersion::SemVer(version)) if version == &self.session.package.version => {
                format!("Approved: v{version}")
            }
            Some(ApprovedVersion::SemVer(version)) => {
                format!(
                    "Version changed: approved v{version} → current v{}",
                    self.session.package.version
                )
            }
            Some(ApprovedVersion::Fingerprint(_)) => {
                "Fingerprint approval: version review unsupported".into()
            }
            None => "Not approved: no previous approval".into(),
        }
    }

    pub fn set_viewport(&mut self, height: u16, width: u16) {
        self.height = usize::from(height);
        self.width = usize::from(width);
        self.top = self.top.min(self.max_top());
        self.left = self.left.min(self.max_left());
    }

    pub fn max_top(&self) -> usize {
        self.file().rows.len().saturating_sub(self.height.max(1))
    }

    fn max_left(&self) -> u16 {
        self.file()
            .width
            .saturating_sub(self.width.max(1))
            .min(u16::MAX as usize) as u16
    }

    pub fn scroll(&mut self, rows: isize, columns: i16) {
        self.change_target = None;
        self.top = self.top.saturating_add_signed(rows).min(self.max_top());
        self.left = self
            .left
            .saturating_add_signed(columns)
            .min(self.max_left());
    }

    pub fn switch_file(&mut self, backwards: bool) {
        self.change_target = None;
        self.selected = if backwards {
            (self.selected + self.files.len() - 1) % self.files.len()
        } else {
            (self.selected + 1) % self.files.len()
        };
        self.top = 0;
        self.left = 0;
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        config::edit::ConfigFile,
        review::{BuildScript, ReviewPackage},
    };

    pub fn fixture(contents: &[&str]) -> (tempfile::TempDir, App) {
        let root = tempfile::tempdir().unwrap();
        let mut scripts = Vec::new();
        for (index, contents) in contents.iter().enumerate() {
            let path = root.path().join(format!("script{index}.rs"));
            fs::write(&path, contents).unwrap();
            scripts.push(BuildScript {
                name: format!("script{index}"),
                path,
            });
        }
        let session = ReviewSession {
            package: ReviewPackage {
                id: cargo_metadata::PackageId {
                    repr: "path+file:///example#0.1.0".into(),
                },
                name: "example".into(),
                version: semver::Version::new(0, 1, 0),
                source: None,
                manifest_path: PathBuf::from(root.path()).join("Cargo.toml"),
                approval: None,
                scripts,
            },
            config: ConfigFile::for_workspace(root.path()).unwrap(),
        };
        (root, App::new(session).unwrap())
    }

    #[test]
    fn change_navigation_wraps_even_when_last_change_is_near_bottom() {
        use crate::review::sources::PackageSources;

        let (_root, mut app) = fixture(&["a\nB\nc\nd\nE\n"]);
        app.set_baseline(Baseline::Ready(PackageSources {
            version: "0.0.1".parse().unwrap(),
            scripts: vec![ScriptSource {
                path: "script0.rs".into(),
                contents: "a\nb\nc\nd\ne\n".into(),
            }],
        }))
        .unwrap();
        app.set_viewport(4, 80);
        let changes = app.file().changes.clone();
        assert_eq!(changes.len(), 2);
        app.jump_change(false);
        assert_eq!(app.change_target, Some(changes[0]));
        app.jump_change(false);
        assert_eq!(app.change_target, Some(changes[1]));
        assert!(app.top < changes[1]);
        app.jump_change(false);
        assert_eq!(app.change_target, Some(changes[0]));
        app.jump_change(true);
        assert_eq!(app.change_target, Some(changes[1]));
    }

    #[test]
    fn scrolling_is_bounded_and_clamped_on_resize() {
        let (_root, mut app) = fixture(&["0123456789\na\nb\nc\nd\n", ""]);
        app.set_viewport(2, 5);
        app.scroll(100, 100);
        assert_eq!((app.top, app.left), (3, 5));
        app.set_viewport(20, 20);
        assert_eq!((app.top, app.left), (0, 0));
        app.scroll(-100, -100);
        assert_eq!((app.top, app.left), (0, 0));
        app.switch_file(false);
        app.scroll(100, 100);
        assert_eq!((app.top, app.left), (0, 0));
        app.switch_file(true);
        assert_eq!(app.selected, 0);
    }
}
