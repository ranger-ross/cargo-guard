use std::{fs, sync::Arc};

use super::{
    ReviewSession,
    diagnostic::Diagnostic,
    diff::{self, FileStatus, ReviewFile},
    highlight::Highlighter,
    selection::Selection,
    sources::{Baseline, ScriptSource},
};
use crate::config::{ApprovalScope, ApprovedVersion};
use eyre::Context;

pub struct App {
    pub session: ReviewSession,
    pub baseline: Baseline,
    pub files: Vec<ReviewFile>,
    current: Arc<[ScriptSource]>,
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
    pub help: Diagnostic,
    pub queue_mode: bool,
    pub decision_blocked: Option<String>,
    pub globally_approved: bool,
    pub selection: Selection,
    pub diagnostic: Diagnostic,
}

impl App {
    pub fn new(session: ReviewSession) -> eyre::Result<Self> {
        let highlighter = Highlighter::new()?;
        let current: Arc<[ScriptSource]> = read_sources(&session)?.into();
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
            help: Diagnostic::default(),
            queue_mode: false,
            decision_blocked: None,
            globally_approved: false,
            selection: Selection::default(),
            diagnostic: Diagnostic::default(),
        })
    }

    pub fn file(&self) -> &ReviewFile {
        &self.files[self.selected]
    }

    pub fn browser_url(&self) -> eyre::Result<String> {
        let file = self.file();
        let source = if file.status == FileStatus::Removed {
            let Baseline::Ready(baseline) = &self.baseline else {
                eyre::bail!("browser source unavailable: removed script has no approved baseline");
            };
            &baseline.browser_source
        } else {
            &self.session.package.browser_source
        };
        source.url(&file.path)
    }

    /// Refresh the edit snapshot without discarding an unchanged baseline.
    pub fn refresh(&mut self, session: ReviewSession) -> eyre::Result<()> {
        self.globally_approved = false;
        self.selection.clear();
        self.diagnostic.reset_view();
        let current: Arc<[ScriptSource]> = read_sources(&session)?.into();
        let invalidated = current != self.current
            || session.config.config().approved.get(&session.package.name)
                != self
                    .session
                    .config
                    .config()
                    .approved
                    .get(&self.session.package.name);
        if invalidated {
            let baseline = Baseline::for_session(&session);
            let files = diff::build_files(&current, &baseline, &self.highlighter)?;
            self.replace_files(baseline, files);
        }
        self.current = current;
        self.session = session;
        if self.is_blanket_approved() {
            self.decision = None;
        }
        self.show_help = false;
        self.notice = None;
        Ok(())
    }

    pub fn refresh_sources(&mut self) -> eyre::Result<bool> {
        let current = read_sources(&self.session)?;
        if current.as_slice() == self.current.as_ref() {
            return Ok(false);
        }
        self.globally_approved = false;
        let baseline = Baseline::for_session(&self.session);
        let files = diff::build_files(&current, &baseline, &self.highlighter)?;
        self.replace_files(baseline, files);
        self.current = current.into();
        Ok(true)
    }

    pub fn set_baseline(&mut self, baseline: Baseline) -> eyre::Result<()> {
        let files = diff::build_files(&self.current, &baseline, &self.highlighter)?;
        self.replace_files(baseline, files);
        Ok(())
    }

    /// Save first; publish the new approval/decision only after persistence
    /// succeeds. Rendering preparation is also done before modifying the file.
    pub fn decide(&mut self, decision: Decision) -> eyre::Result<()> {
        if let Some(reason) = &self.decision_blocked {
            eyre::bail!("{reason}");
        }
        eyre::ensure!(
            !self.is_blanket_approved(),
            "blanket approval must be changed in the config file"
        );
        let revoke = self.is_current_version_approved();
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
                self.session
                    .config
                    .add_version(&package.name, &package.version)?;
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
                    "Rejected {} v{}; its approval was removed from {}",
                    package.name,
                    package.version,
                    self.session.config.path().display()
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
            message: format!("{error:#}"),
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
            message.push_str(" · Build script entry points match the approved version");
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

    pub fn is_blanket_approved(&self) -> bool {
        self.session
            .package
            .approval
            .as_ref()
            .is_some_and(ApprovedVersion::is_blanket)
    }

    pub fn is_current_version_approved(&self) -> bool {
        matches!(
            &self.session.package.approval,
            Some(ApprovedVersion::SemVer(version)) if version == &self.session.package.version
        )
    }

    /// Display supplemental approval without changing the selected write destination.
    pub fn displayed_approval_scope(&self) -> Option<ApprovalScope> {
        if self.is_blanket_approved() || self.is_current_version_approved() {
            Some(self.session.config.target().scope)
        } else if self.globally_approved {
            Some(ApprovalScope::Global)
        } else {
            None
        }
    }

    pub fn can_decide(&self) -> bool {
        self.decision_blocked.is_none()
            && !self.is_blanket_approved()
            && !self.is_current_version_approved()
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
            Some(ApprovedVersion::Blanket(_)) => {
                "Approved: all versions (manual blanket approval)".into()
            }
            Some(ApprovedVersion::SemVer(version)) if version == &self.session.package.version => {
                format!("Approved: v{version}")
            }
            Some(ApprovedVersion::SemVer(version)) => {
                format!(
                    "Version changed: approved v{version} → current v{}",
                    self.session.package.version
                )
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

    pub fn scroll(&mut self, rows: isize, columns: i16) {
        self.change_target = None;
        self.top = self.top.saturating_add_signed(rows).min(self.max_top());
        self.left = self
            .left
            .saturating_add_signed(columns)
            .min(self.max_left());
    }

    pub fn switch_file(&mut self, backwards: bool) {
        self.selection.clear();
        self.change_target = None;
        self.selected = if backwards {
            (self.selected + self.files.len() - 1) % self.files.len()
        } else {
            (self.selected + 1) % self.files.len()
        };
        self.top = 0;
        self.left = 0;
    }

    fn replace_files(&mut self, baseline: Baseline, files: Vec<ReviewFile>) {
        self.selection.clear();
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

    fn max_left(&self) -> u16 {
        self.file()
            .width
            .saturating_sub(self.width.max(1))
            .min(u16::MAX as usize) as u16
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

pub struct Notice {
    pub message: String,
    pub is_error: bool,
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
            workspace_root: root.path().to_path_buf(),
            policy_notes: Vec::new(),
            package: ReviewPackage {
                id: cargo_metadata::PackageId {
                    repr: "path+file:///example#0.1.0".into(),
                },
                name: "example".into(),
                version: semver::Version::new(0, 1, 0),
                source: None,
                browser_source: Default::default(),
                manifest_path: PathBuf::from(root.path()).join("Cargo.toml"),
                approval: None,
                scripts,
            },
            config: ConfigFile::for_workspace(root.path()).unwrap(),
        };
        (root, App::new(session).unwrap())
    }

    fn registry_browser_source(version: &str) -> super::super::browser::BrowserSource {
        let package: cargo_metadata::Package = serde_json::from_value(serde_json::json!({
            "name": "example",
            "version": version,
            "id": format!("registry+https://github.com/rust-lang/crates.io-index#example@{version}"),
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "manifest_path": "/nonexistent/cargo-guard-browser-fixture/Cargo.toml",
            "dependencies": [],
            "targets": [],
            "features": {},
            "authors": [],
            "edition": "2021",
            "metadata": null
        }))
        .unwrap();
        super::super::browser::BrowserSource::from_package(&package)
    }

    #[test]
    fn browser_url_routes_selected_scripts_without_changing_review_state() {
        use crate::review::sources::PackageSources;

        let (_root, mut app) = fixture(&["new\n", "same\n", "added\n"]);
        app.session.package.browser_source = registry_browser_source("0.1.0");
        app.set_baseline(Baseline::CurrentVersion).unwrap();
        app.selected = 2;
        assert_eq!(app.file().status, FileStatus::Current);
        assert_eq!(
            app.browser_url().unwrap(),
            "https://docs.rs/crate/example/0.1.0/source/script2.rs"
        );
        app.set_baseline(Baseline::Ready(PackageSources {
            version: "0.0.1".parse().unwrap(),
            browser_source: registry_browser_source("0.0.1"),
            scripts: vec![
                ScriptSource {
                    path: "script0.rs".into(),
                    contents: "old\n".into(),
                },
                ScriptSource {
                    path: "script1.rs".into(),
                    contents: "same\n".into(),
                },
                ScriptSource {
                    path: "removed.rs".into(),
                    contents: "removed\n".into(),
                },
            ],
        }))
        .unwrap();
        let approval = ApprovedVersion::SemVer("0.1.0".parse().unwrap());
        app.session.package.approval = Some(approval.clone());
        app.decision_blocked = Some("read-only review".into());
        assert!(!app.can_decide());
        let saved_policy = fs::read(app.session.config.path()).ok();
        for (path, status, version) in [
            ("script0.rs", FileStatus::Modified, "0.1.0"),
            ("script1.rs", FileStatus::Unchanged, "0.1.0"),
            ("script2.rs", FileStatus::Added, "0.1.0"),
            ("removed.rs", FileStatus::Removed, "0.0.1"),
        ] {
            app.selected = app
                .files
                .iter()
                .position(|file| file.path == std::path::Path::new(path))
                .unwrap();
            let selected = app.selected;
            assert_eq!(app.file().status, status);
            assert_eq!(
                app.browser_url().unwrap(),
                format!("https://docs.rs/crate/example/{version}/source/{path}")
            );
            assert_eq!(app.selected, selected);
            assert_eq!(app.session.package.approval, Some(approval.clone()));
            assert_eq!(app.decision, None);
            assert_eq!(fs::read(app.session.config.path()).ok(), saved_policy);
        }

        let Baseline::Ready(baseline) = &mut app.baseline else {
            unreachable!();
        };
        baseline.browser_source = Default::default();
        assert!(app.browser_url().is_err());
        app.baseline = Baseline::NewPackage;
        assert!(
            app.browser_url()
                .unwrap_err()
                .to_string()
                .contains("approved baseline")
        );
    }

    #[test]
    fn browser_url_reports_unavailable_current_source() {
        let (_root, app) = fixture(&["fn main() {}\n"]);
        assert!(app.browser_url().is_err());
        assert_eq!(app.selected, 0);
        assert!(app.session.package.approval.is_none());
        assert_eq!(app.decision, None);
    }

    #[test]
    fn manual_blanket_approval_survives_version_and_source_changes() {
        let (root, app) = fixture(&["fn main() {}\n"]);
        let path = app.session.config.path().to_path_buf();
        let policy = "# Manual approval\n[approved]\nexample = 'approved'\n";
        fs::write(&path, policy).unwrap();
        let mut session = app.session;
        session.config = ConfigFile::open(session.config.target().clone()).unwrap();
        session.package.approval =
            session.config.config().approved["example"].for_review(&session.package.version);
        let mut app = App::new(session).unwrap();
        for version in ["0.1.0", "9.8.7"] {
            app.session.package.version = version.parse().unwrap();
            assert!(app.is_blanket_approved());
            assert!(!app.can_decide());
            assert_eq!(
                app.displayed_approval_scope(),
                Some(ApprovalScope::Workspace)
            );
            assert!(app.approval_status().contains("all versions"));
            assert!(matches!(app.baseline, Baseline::Blanket));
            assert!(
                super::super::sources::BaselineLoader::start(&app.session)
                    .unwrap()
                    .is_none()
            );
            for decision in [Decision::Approve, Decision::Reject] {
                assert!(app.decide(decision).is_err());
                assert_eq!(app.decision, None);
                assert_eq!(fs::read_to_string(&path).unwrap(), policy);
            }
        }
        fs::write(&app.session.package.scripts[0].path, "changed inputs\n").unwrap();
        assert!(app.refresh_sources().unwrap());
        assert!(matches!(app.baseline, Baseline::Blanket));
        assert!(!app.can_decide());
        assert!(!root.path().join("target").exists());
    }

    #[test]
    fn global_indicator_keeps_workspace_approval_actionable_and_local() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        assert!(!app.globally_approved);
        app.globally_approved = true;
        assert_eq!(app.displayed_approval_scope(), Some(ApprovalScope::Global));
        assert!(app.can_decide());
        assert!(app.session.package.approval.is_none());
        assert!(matches!(app.baseline, Baseline::NewPackage));
        let destination = app.session.config.path().to_path_buf();

        app.decide(Decision::Approve).unwrap();
        assert_eq!(app.session.config.path(), destination);
        assert_eq!(app.session.config.target().scope, ApprovalScope::Workspace);
        assert_eq!(
            app.displayed_approval_scope(),
            Some(ApprovalScope::Workspace)
        );
        let saved = crate::config::load_config_from(&destination).unwrap();
        assert!(
            matches!(&saved.approved["example"], ApprovedVersion::SemVer(v) if v == &app.session.package.version)
        );
        assert!(!app.can_decide());

        app.decide(Decision::Reject).unwrap();
        assert!(app.globally_approved);
        assert!(app.session.package.approval.is_none());
        assert!(
            crate::config::load_config_from(&destination)
                .unwrap()
                .approved
                .is_empty()
        );
        assert_eq!(app.decision, Some(Decision::Reject));
    }

    #[test]
    fn rejecting_supplemental_approval_does_not_create_workspace_policy() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        app.globally_approved = true;
        app.decide(Decision::Reject).unwrap();
        assert!(!app.session.config.path().exists());
        assert!(app.globally_approved);
        assert!(app.session.package.approval.is_none());
        assert!(matches!(app.baseline, Baseline::NewPackage));
    }

    #[test]
    fn source_changes_clear_supplemental_approval() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        app.globally_approved = true;
        assert!(!app.refresh_sources().unwrap());
        assert!(app.globally_approved);
        fs::write(&app.session.package.scripts[0].path, "fn main() { }").unwrap();
        assert!(app.refresh_sources().unwrap());
        assert!(!app.globally_approved);
        assert_eq!(app.displayed_approval_scope(), None);
    }

    #[test]
    fn refreshed_sessions_require_supplemental_approval_to_be_reestablished() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        let (_next_root, next) = fixture(&["fn main() {}\n"]);
        app.globally_approved = true;
        app.refresh(next.session).unwrap();
        assert!(!app.globally_approved);
        assert!(app.can_decide());
        assert_eq!(app.displayed_approval_scope(), None);
    }

    #[test]
    fn pins_package_then_revokes_its_current_version() {
        let (_root, mut app) = fixture(&["fn main() {}\n", "// second script\n"]);
        app.decide(Decision::Approve).unwrap();
        let saved = crate::config::load_config_from(app.session.config.path()).unwrap();
        assert!(
            matches!(&saved.approved["example"], ApprovedVersion::SemVer(v) if *v == app.session.package.version)
        );
        assert_eq!(saved.approved.len(), 1);
        assert_eq!(app.decision, Some(Decision::Approve));
        assert!(matches!(app.baseline, Baseline::CurrentVersion));

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
    }

    #[test]
    fn rejecting_new_or_upgraded_versions_keeps_config_unchanged() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        app.decide(Decision::Reject).unwrap();
        assert!(!app.session.config.path().exists());

        app.session
            .config
            .add_version("example", &"0.0.1".parse().unwrap())
            .unwrap();
        let mut app = App::new(app.session).unwrap();
        // Reconstruct the package's approval as discovery would do on startup.
        app.session.package.approval = app.session.config.config().approved.get("example").cloned();
        app.baseline = Baseline::for_session(&app.session);
        let before = fs::read(app.session.config.path()).unwrap();
        app.decide(Decision::Reject).unwrap();
        assert_eq!(fs::read(app.session.config.path()).unwrap(), before);
        assert!(matches!(app.baseline, Baseline::Loading(_)));
        app.decide(Decision::Approve).unwrap();
        assert!(
            matches!(&app.session.package.approval, Some(ApprovedVersion::SemVer(v)) if *v == app.session.package.version)
        );
    }

    #[test]
    fn version_lists_keep_other_entries_when_approving_and_rejecting() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        let name = app.session.package.name.clone();
        for version in ["0.0.1", "0.0.2"] {
            app.session
                .config
                .add_version(&name, &version.parse().unwrap())
                .unwrap();
        }
        app.session.package.approval =
            app.session.config.config().approved[&name].for_review(&app.session.package.version);
        assert!(app.can_decide());
        app.decide(Decision::Approve).unwrap();
        let approvals = &app.session.config.config().approved[&name];
        assert!(approvals.allows_version(&"0.0.1".parse().unwrap()));
        assert!(approvals.allows_version(&"0.0.2".parse().unwrap()));
        assert!(approvals.allows_version(&app.session.package.version));
        app.decide(Decision::Reject).unwrap();
        let approvals = &app.session.config.config().approved[&name];
        assert!(approvals.allows_version(&"0.0.1".parse().unwrap()));
        assert!(approvals.allows_version(&"0.0.2".parse().unwrap()));
        assert!(!approvals.allows_version(&app.session.package.version));
    }

    #[test]
    fn global_version_removal_and_failed_saves_leave_workspace_policy_untouched() {
        use crate::config::{ApprovalScope, ApprovalTarget};

        let (root, mut app) = fixture(&["fn main() {}\n"]);
        let workspace = root.path().join("approved-builds.toml");
        let workspace_policy = "[approved]\nexample = '0.1.0'\n";
        fs::write(&workspace, workspace_policy).unwrap();
        let global = tempfile::tempdir().unwrap();
        let path = global.path().join("approved-builds.toml");
        app.session.config = ConfigFile::open(ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Global,
        })
        .unwrap();
        fs::write(&path, "[approved]\nother = '1.0.0'\n").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(app.decide(Decision::Approve).is_err());
        assert_eq!(app.decision, None);
        assert!(app.session.package.approval.is_none());
        assert_eq!(fs::read(&path).unwrap(), before);

        app.session.config = ConfigFile::open(app.session.config.target().clone()).unwrap();
        app.decide(Decision::Approve).unwrap();
        app.decide(Decision::Reject).unwrap();
        let saved = crate::config::load_config_from(&path).unwrap();
        assert!(!saved.approved.contains_key("example"));
        assert!(saved.approved.contains_key("other"));
        assert_eq!(fs::read_to_string(&workspace).unwrap(), workspace_policy);
        assert_eq!(app.session.workspace_root, root.path());
        assert_eq!(app.session.config.target().scope, ApprovalScope::Global);
    }

    #[test]
    fn save_failures_leave_decisions_and_approvals_unchanged() {
        let (_root, mut app) = fixture(&["fn main() {}\n"]);
        fs::write(app.session.config.path(), "[approved]\nother = '1.0.0'\n").unwrap();
        let before = fs::read(app.session.config.path()).unwrap();
        let error = app.decide(Decision::Approve).unwrap_err();
        app.report_decision_error(error);
        assert!(app.notice.as_ref().unwrap().is_error);
        assert_eq!(app.decision, None);
        assert!(app.session.package.approval.is_none());
        assert_eq!(fs::read(app.session.config.path()).unwrap(), before);

        app.session.config = ConfigFile::open(app.session.config.target().clone()).unwrap();
        app.decide(Decision::Approve).unwrap();
        let changed = format!(
            "{}\n# external edit\n",
            fs::read_to_string(app.session.config.path()).unwrap()
        );
        fs::write(app.session.config.path(), &changed).unwrap();
        assert!(app.decide(Decision::Reject).is_err());
        assert_eq!(app.decision, Some(Decision::Approve));
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
            browser_source: Default::default(),
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
