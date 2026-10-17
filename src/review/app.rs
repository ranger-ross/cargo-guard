use std::fs;

use super::{
    ReviewSession,
    diff::{self, FileStatus, ReviewFile},
    highlight::Highlighter,
    sources::{Baseline, ScriptSource},
};
use crate::config::ApprovedVersion;
use eyre::Context;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

pub struct Notice {
    pub message: String,
    pub is_error: bool,
}

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
    pub decision: Option<Decision>,
    pub notice: Option<Notice>,
    pub show_help: bool,
    pub queue_mode: bool,
    pub decision_blocked: Option<String>,
}

impl App {
    pub fn new(session: ReviewSession) -> eyre::Result<Self> {
        let highlighter = Highlighter::new()?;
        let current = read_sources(&session)?;
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
            decision: None,
            notice: None,
            show_help: false,
            queue_mode: false,
            decision_blocked: None,
        })
    }

    pub fn file(&self) -> &ReviewFile {
        &self.files[self.selected]
    }

    /// Reuse expensive highlighted/diffed rows, but always refresh the edit
    /// snapshot and compare source contents before displaying a cached view.
    pub fn refresh(&mut self, session: ReviewSession) -> eyre::Result<()> {
        let current = read_sources(&session)?;
        let invalidated =
            current != self.current || session.package.approval != self.session.package.approval;
        if invalidated || matches!(self.baseline, Baseline::Unavailable { .. }) {
            let baseline = Baseline::for_session(&session);
            let files = diff::build_files(&current, &baseline, &self.highlighter)?;
            self.replace_files(baseline, files);
        }
        self.current = current;
        self.session = session;
        self.show_help = false;
        self.notice = None;
        Ok(())
    }

    pub fn set_baseline(&mut self, baseline: Baseline) -> eyre::Result<()> {
        let files = diff::build_files(&self.current, &baseline, &self.highlighter)?;
        self.replace_files(baseline, files);
        Ok(())
    }

    fn replace_files(&mut self, baseline: Baseline, files: Vec<ReviewFile>) {
        self.selected = files
            .iter()
            .position(|file| file.path == self.file().path)
            .unwrap_or(0);
        self.files = files;
        self.baseline = baseline;
        self.top = 0;
        self.left = 0;
        self.change_target = None;
    }

    /// Save first; publish the new approval/decision only after persistence
    /// succeeds. Rendering preparation is also done before modifying the file.
    pub fn decide(&mut self, decision: Decision) -> eyre::Result<()> {
        if let Some(reason) = &self.decision_blocked {
            eyre::bail!("{reason}");
        }
        eyre::ensure!(
            !matches!(
                self.session.package.approval,
                Some(ApprovedVersion::Fingerprint(_))
            ),
            "fingerprint approvals cannot be changed by version review"
        );
        let revoke = matches!(
            &self.session.package.approval,
            Some(ApprovedVersion::SemVer(version)) if version == &self.session.package.version
        );
        let next_baseline = match decision {
            Decision::Approve => Some(Baseline::CurrentVersion),
            Decision::Reject if revoke => Some(Baseline::NewPackage),
            Decision::Reject => None,
        };
        let prepared = next_baseline
            .map(|baseline| {
                diff::build_files(&self.current, &baseline, &self.highlighter)
                    .map(|files| (baseline, files))
            })
            .transpose()?;
        let package = &self.session.package;
        let message = match decision {
            Decision::Approve => {
                self.session.config.set_approval(
                    &package.name,
                    ApprovedVersion::SemVer(package.version.clone()),
                )?;
                format!(
                    "Approved {} v{}; saved to {}",
                    package.name,
                    package.version,
                    self.session.config.path().display()
                )
            }
            Decision::Reject if revoke => {
                self.session
                    .config
                    .remove_version(&package.name, &package.version)?;
                format!(
                    "Rejected {} v{}; its approval was removed",
                    package.name, package.version
                )
            }
            Decision::Reject => match &package.approval {
                Some(ApprovedVersion::SemVer(version)) => format!(
                    "Rejected {} v{}; approval for v{version} retained",
                    package.name, package.version
                ),
                _ => format!(
                    "Rejected {} v{}; no approval was added",
                    package.name, package.version
                ),
            },
        };
        self.session.package.approval = self
            .session
            .config
            .config()
            .approved
            .get(&self.session.package.name)
            .and_then(|a| a.for_review(&self.session.package.version));
        if let Some((baseline, files)) = prepared {
            self.replace_files(baseline, files);
        }
        self.decision = Some(decision);
        self.notice = Some(Notice {
            message,
            is_error: false,
        });
        Ok(())
    }

    pub fn report_decision_error(&mut self, error: eyre::Report) {
        self.notice = Some(Notice {
            message: format!("Decision failed: {error:#}"),
            is_error: true,
        });
    }

    pub fn baseline_message(&self) -> String {
        let mut message = self.baseline.message();
        if matches!(self.baseline, Baseline::Ready(_))
            && self
                .files
                .iter()
                .all(|file| file.status == FileStatus::Unchanged)
        {
            message.push_str(" · Build scripts unchanged; new version still needs approval");
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

    pub fn is_current_version_approved(&self) -> bool {
        matches!(
            &self.session.package.approval,
            Some(ApprovedVersion::SemVer(version)) if version == &self.session.package.version
        )
    }

    pub fn can_decide(&self) -> bool {
        self.decision_blocked.is_none()
            && !self.is_current_version_approved()
            && !matches!(
                self.session.package.approval,
                Some(ApprovedVersion::Fingerprint(_))
            )
    }

    pub fn approval_status(&self) -> String {
        if let Some(reason) = &self.decision_blocked {
            return format!("Read-only: {reason}");
        }
        if self.decision == Some(Decision::Reject) {
            return match &self.session.package.approval {
                Some(ApprovedVersion::SemVer(version)) => format!(
                    "Rejected current v{}; v{version} remains approved",
                    self.session.package.version
                ),
                _ => format!(
                    "Rejected: v{} is not approved",
                    self.session.package.version
                ),
            };
        }
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
            Some(ApprovedVersion::Multiple(_)) => {
                unreachable!("review selects a representative approval")
            }
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

fn read_sources(session: &ReviewSession) -> eyre::Result<Vec<ScriptSource>> {
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
    Ok(current)
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
    fn approves_package_then_revokes_its_current_version() {
        let (_root, mut app) = fixture(&["fn main() {}\n", "// second script\n"]);
        app.decide(Decision::Approve).unwrap();
        let saved = crate::config::load_config_from(app.session.config.path()).unwrap();
        assert!(
            matches!(&saved.approved["example"], ApprovedVersion::SemVer(v) if *v == app.session.package.version)
        );
        assert_eq!(saved.approved.len(), 1);
        assert_eq!(app.decision, Some(Decision::Approve));
        assert!(matches!(app.baseline, Baseline::CurrentVersion));
        assert_eq!(app.approval_status(), "Approved: v0.1.0");

        app.decide(Decision::Reject).unwrap();
        assert!(
            crate::config::load_config_from(app.session.config.path())
                .unwrap()
                .approved
                .is_empty()
        );
        assert!(app.session.package.approval.is_none());
        assert_eq!(app.decision, Some(Decision::Reject));
        assert!(matches!(app.baseline, Baseline::NewPackage));
        assert!(app.approval_status().contains("Rejected"));
    }

    #[test]
    fn rejecting_new_or_upgraded_versions_keeps_config_unchanged() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        app.decide(Decision::Reject).unwrap();
        assert!(!app.session.config.path().exists());

        app.session
            .config
            .set_approval("example", ApprovedVersion::SemVer("0.0.1".parse().unwrap()))
            .unwrap();
        let mut app = App::new(app.session).unwrap();
        // Reconstruct the package's approval as discovery would do on startup.
        app.session.package.approval = app.session.config.config().approved.get("example").cloned();
        app.baseline = Baseline::for_session(&app.session);
        let before = fs::read(app.session.config.path()).unwrap();
        app.decide(Decision::Reject).unwrap();
        assert_eq!(fs::read(app.session.config.path()).unwrap(), before);
        assert!(app.approval_status().contains("v0.0.1 remains approved"));
        assert!(matches!(app.baseline, Baseline::Loading(_)));
        app.decide(Decision::Approve).unwrap();
        assert!(
            matches!(&app.session.package.approval, Some(ApprovedVersion::SemVer(v)) if *v == app.session.package.version)
        );
    }

    #[test]
    fn fingerprint_approvals_are_not_overwritten_or_revoked() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        app.session
            .config
            .set_approval("example", ApprovedVersion::Fingerprint("abc123".into()))
            .unwrap();
        app.session.package.approval = app.session.config.config().approved.get("example").cloned();
        let before = fs::read(app.session.config.path()).unwrap();
        for decision in [Decision::Approve, Decision::Reject] {
            assert!(app.decide(decision).is_err());
            assert_eq!(fs::read(app.session.config.path()).unwrap(), before);
            assert_eq!(app.decision, None);
        }
    }

    #[test]
    fn mixed_approvals_allow_version_review_and_preserve_other_entries() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        let name = app.session.package.name.clone();
        app.session
            .config
            .set_approval(&name, ApprovedVersion::Fingerprint("script-hash".into()))
            .unwrap();
        app.session
            .config
            .set_approval(&name, ApprovedVersion::SemVer("0.0.1".parse().unwrap()))
            .unwrap();
        app.session.package.approval =
            app.session.config.config().approved[&name].for_review(&app.session.package.version);
        assert!(app.can_decide());
        app.decide(Decision::Approve).unwrap();
        let approvals = &app.session.config.config().approved[&name];
        assert!(approvals.allows_fingerprint("script-hash"));
        assert!(approvals.allows_version(&"0.0.1".parse().unwrap()));
        assert!(approvals.allows_version(&app.session.package.version));
        app.decide(Decision::Reject).unwrap();
        let approvals = &app.session.config.config().approved[&name];
        assert!(approvals.allows_fingerprint("script-hash"));
        assert!(approvals.allows_version(&"0.0.1".parse().unwrap()));
        assert!(!approvals.allows_version(&app.session.package.version));
    }

    #[test]
    fn save_failures_leave_decisions_and_approvals_unchanged() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        fs::write(app.session.config.path(), "[approved]\nother = '1.0.0'\n").unwrap();
        let before = fs::read(app.session.config.path()).unwrap();
        let error = app.decide(Decision::Approve).unwrap_err();
        app.report_decision_error(error);
        assert!(app.notice.as_ref().unwrap().is_error);
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .message
                .contains("changed since it was loaded")
        );
        assert_eq!(app.decision, None);
        assert!(app.session.package.approval.is_none());
        assert_eq!(fs::read(app.session.config.path()).unwrap(), before);

        app.session.config = ConfigFile::open(app.session.config.path()).unwrap();
        app.decide(Decision::Approve).unwrap();
        let changed = format!(
            "{}\n# external edit\n",
            fs::read_to_string(app.session.config.path()).unwrap()
        );
        fs::write(app.session.config.path(), &changed).unwrap();
        assert!(app.decide(Decision::Reject).is_err());
        assert_eq!(app.decision, Some(Decision::Approve));
        assert_eq!(app.approval_status(), "Approved: v0.1.0");
        assert_eq!(
            fs::read_to_string(app.session.config.path()).unwrap(),
            changed
        );
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
