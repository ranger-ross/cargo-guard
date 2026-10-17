use std::fs;

use eyre::Context;
use ratatui::text::Line;

use super::{ReviewSession, highlight::Highlighter, sources::Baseline};
use crate::config::ApprovedVersion;

pub struct App {
    pub session: ReviewSession,
    pub baseline: Baseline,
    pub files: Vec<SourceFile>,
    pub selected: usize,
    pub top: usize,
    pub left: u16,
    pub height: usize,
    pub width: usize,
}

pub struct SourceFile {
    pub label: String,
    pub lines: Vec<Line<'static>>,
    pub width: usize,
}

impl App {
    pub fn new(session: ReviewSession) -> eyre::Result<Self> {
        let highlighter = Highlighter::new()?;
        let root = session.package.manifest_path.parent();
        let files = session
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
                let lines = highlighter.highlight(&contents).with_context(|| {
                    format!("could not highlight build script {}", script.path.display())
                })?;
                let width = lines.iter().map(|line| line.width()).max().unwrap_or(0);
                Ok(SourceFile {
                    label: format!("{} ({})", path.display(), script.name),
                    lines,
                    width,
                })
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        eyre::ensure!(!files.is_empty(), "no build scripts to display");
        Ok(Self {
            baseline: Baseline::for_session(&session),
            session,
            files,
            selected: 0,
            top: 0,
            left: 0,
            height: 0,
            width: 0,
        })
    }

    pub fn file(&self) -> &SourceFile {
        &self.files[self.selected]
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
        self.file().lines.len().saturating_sub(self.height.max(1))
    }

    fn max_left(&self) -> u16 {
        self.file()
            .width
            .saturating_sub(self.width.max(1))
            .min(u16::MAX as usize) as u16
    }

    pub fn scroll(&mut self, rows: isize, columns: i16) {
        self.top = self.top.saturating_add_signed(rows).min(self.max_top());
        self.left = self
            .left
            .saturating_add_signed(columns)
            .min(self.max_left());
    }

    pub fn switch_file(&mut self, backwards: bool) {
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
    fn scrolling_is_bounded_and_clamped_on_resize() {
        let (_root, mut app) = fixture(&["0123456789\na\nb\nc\nd", ""]);
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
