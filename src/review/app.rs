use std::{fs, sync::Arc};

use super::{
    ReviewSession,
    diagnostic::Diagnostic,
    diff::{self, FileStatus, ReviewFile},
    highlight::Highlighter,
    pin_dialog::PinDialog,
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
    pub verification_message: Option<String>,
    pub verification_pending: bool,
    pub globally_approved: bool,
    pub selection: Selection,
    pub diagnostic: Diagnostic,
    pub pin_dialog: Option<PinDialog>,
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
            verification_message: None,
            verification_pending: false,
            globally_approved: false,
            selection: Selection::default(),
            diagnostic: Diagnostic::default(),
            pin_dialog: None,
        })
    }

    pub fn file(&self) -> &ReviewFile {
        &self.files[self.selected]
    }

    /// Refresh the edit snapshot without discarding unchanged verified sources.
    pub fn refresh(&mut self, session: ReviewSession) -> eyre::Result<()> {
        self.globally_approved = false;
        self.pin_dialog = None;
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
            if self.decision == Some(Decision::ApproveFingerprint) {
                self.decision = None;
            }
            self.verification_message = None;
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

    pub fn sources(&self) -> Arc<[ScriptSource]> {
        Arc::clone(&self.current)
    }

    pub fn refresh_sources(&mut self) -> eyre::Result<bool> {
        let current = read_sources(&self.session)?;
        if current.as_slice() == self.current.as_ref() {
            return Ok(false);
        }
        self.globally_approved = false;
        self.pin_dialog = None;
        let baseline = Baseline::for_session(&self.session);
        let files = diff::build_files(&current, &baseline, &self.highlighter)?;
        self.replace_files(baseline, files);
        self.current = current.into();
        if self.decision == Some(Decision::ApproveFingerprint) {
            self.decision = None;
        }
        Ok(true)
    }

    pub fn set_baseline(&mut self, baseline: Baseline) -> eyre::Result<()> {
        if let Baseline::CurrentFingerprint(verified) = &baseline {
            eyre::ensure!(
                verified == &self.current,
                "verified build script sources differ from the displayed sources; reopen review"
            );
        }
        if let (Baseline::CurrentFingerprint(old), Baseline::CurrentFingerprint(new)) =
            (&self.baseline, &baseline)
            && old == new
        {
            return Ok(());
        }
        let files = diff::build_files(&self.current, &baseline, &self.highlighter)?;
        self.replace_files(baseline, files);
        Ok(())
    }

    /// Save first; publish the new approval/decision only after persistence
    /// succeeds. Rendering preparation is also done before modifying the file.
    pub fn decide(&mut self, decision: Decision) -> eyre::Result<bool> {
        if let Some(reason) = &self.decision_blocked {
            eyre::bail!("{reason}");
        }
        eyre::ensure!(
            !self.is_blanket_approved(),
            "all versions and fingerprints are manually approved; edit the configuration to change this approval"
        );
        if decision == Decision::ApproveFingerprint {
            return self.approve_fingerprints();
        }
        let revoke = matches!(
            &self.session.package.approval,
            Some(ApprovedVersion::SemVer(version)) if version == &self.session.package.version
        );
        let next_baseline = match decision {
            Decision::PinVersion => Some(Baseline::CurrentVersion),
            Decision::Reject if revoke => Some(Baseline::NewPackage),
            Decision::Reject => None,
            Decision::ApproveFingerprint => unreachable!(),
        };
        let prepared = next_baseline
            .map(|baseline| {
                diff::build_files(&self.current, &baseline, &self.highlighter)
                    .map(|files| (baseline, files))
            })
            .transpose()?;
        let package = &self.session.package;
        let message = match decision {
            Decision::ApproveFingerprint => unreachable!(),
            Decision::PinVersion => {
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
        self.pin_dialog = None;
        self.notice = Some(Notice {
            message,
            is_error: false,
        });
        Ok(true)
    }

    pub fn report_decision_error(&mut self, error: eyre::Report) {
        if self.pin_dialog.is_some() {
            self.pin_dialog = Some(PinDialog::new(format!("{error:#}")));
        } else {
            self.notice = Some(Notice {
                message: format!("{error:#}"),
                is_error: true,
            });
        }
    }

    pub fn baseline_message(&self) -> String {
        if self.is_current_fingerprint_approved() {
            return "Baseline: current build-script fingerprints are approved".into();
        }
        if let Some(message) = &self.verification_message {
            return message.clone();
        }
        let mut message = self.baseline.message();
        if matches!(self.baseline, Baseline::Ready(_))
            && self
                .files
                .iter()
                .all(|file| file.status == FileStatus::Unchanged)
        {
            message.push_str(
                " · Build script entry points unchanged; current inputs still need approval or verification",
            );
        }
        message
    }

    pub fn jump_change(&mut self, backwards: bool) {
        self.pin_dialog = None;
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

    pub fn is_current_fingerprint_approved(&self) -> bool {
        matches!(self.baseline, Baseline::CurrentFingerprint(_))
            || self.decision == Some(Decision::ApproveFingerprint)
    }

    /// Display supplemental approval without changing the selected write destination.
    pub fn displayed_approval_scope(&self) -> Option<ApprovalScope> {
        if self.is_blanket_approved()
            || self.is_current_version_approved()
            || self.is_current_fingerprint_approved()
        {
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
            && !self.is_current_fingerprint_approved()
    }

    pub fn approval_status(&self) -> String {
        if self.is_current_fingerprint_approved() {
            return "Approved current build-script fingerprints".into();
        }
        if let Some(message) = &self.verification_message {
            return message.clone();
        }
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
                "Approved: all versions and fingerprints (manual blanket approval)".into()
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
            Some(ApprovedVersion::Fingerprint(_)) => {
                "Current build-script inputs need fingerprint approval".into()
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
        self.pin_dialog = None;
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

    fn approve_fingerprints(&mut self) -> eyre::Result<bool> {
        eyre::ensure!(
            self.can_decide(),
            "this package is already approved or read-only"
        );
        self.pin_dialog = None;
        eyre::ensure!(
            read_sources(&self.session)?.as_slice() == self.current.as_ref(),
            "build script sources changed since opening review; reopen review before approving"
        );
        let approvals = super::fingerprint::approvals(&self.session);
        eyre::ensure!(
            read_sources(&self.session)?.as_slice() == self.current.as_ref(),
            "build script sources changed since opening review; reopen review before approving"
        );
        let approvals = match approvals {
            Ok(approvals) => approvals,
            Err(error) => {
                self.pin_dialog = Some(PinDialog::new(format!("{error:#}")));
                return Ok(false);
            }
        };
        self.session
            .config
            .add_approvals(&self.session.package.name, &approvals)?;
        self.session.package.approval = self
            .session
            .config
            .config()
            .approved
            .get(&self.session.package.name)
            .and_then(|a| a.for_review(&self.session.package.version));
        self.decision = Some(Decision::ApproveFingerprint);
        self.pin_dialog = None;
        self.notice = Some(Notice {
            message: format!(
                "Approved {} fingerprint(s) for {} v{}; saved to {}",
                approvals.len(),
                self.session.package.name,
                self.session.package.version,
                self.session.config.path().display()
            ),
            is_error: false,
        });
        Ok(true)
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
    PinVersion,
    ApproveFingerprint,
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
                manifest_path: PathBuf::from(root.path()).join("Cargo.toml"),
                approval: None,
                scripts,
            },
            config: ConfigFile::for_workspace(root.path()).unwrap(),
        };
        (root, App::new(session).unwrap())
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
            assert!(
                app.approval_status()
                    .contains("all versions and fingerprints")
            );
            assert!(matches!(app.baseline, Baseline::Blanket));
            assert!(
                super::super::sources::BaselineLoader::start(&app.session)
                    .unwrap()
                    .is_none()
            );
            for decision in [
                Decision::ApproveFingerprint,
                Decision::PinVersion,
                Decision::Reject,
            ] {
                assert!(app.decide(decision).is_err());
                assert_eq!(app.decision, None);
                assert!(app.pin_dialog.is_none());
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

        app.decide(Decision::PinVersion).unwrap();
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
        assert!(app.decide(Decision::PinVersion).unwrap());
        let saved = crate::config::load_config_from(app.session.config.path()).unwrap();
        assert!(
            matches!(&saved.approved["example"], ApprovedVersion::SemVer(v) if *v == app.session.package.version)
        );
        assert_eq!(saved.approved.len(), 1);
        assert_eq!(app.decision, Some(Decision::PinVersion));
        assert!(matches!(app.baseline, Baseline::CurrentVersion));

        assert!(app.decide(Decision::Reject).unwrap());
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
        assert!(app.decide(Decision::Reject).unwrap());
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
        assert!(matches!(app.baseline, Baseline::Loading(_)));
        assert!(app.decide(Decision::PinVersion).unwrap());
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
        assert!(!app.decide(Decision::ApproveFingerprint).unwrap());
        assert!(app.pin_dialog.is_some());
        assert_eq!(app.decision, None);
        app.decide(Decision::Reject).unwrap();
        assert_eq!(fs::read(app.session.config.path()).unwrap(), before);
    }

    #[test]
    fn verified_current_fingerprints_are_read_only_without_a_new_decision() {
        let (_root, mut app) = fixture(&["fn main() {}\n", "// second script\n"]);
        assert!(app.can_decide());
        app.set_baseline(Baseline::CurrentFingerprint(
            read_sources(&app.session).unwrap().into(),
        ))
        .unwrap();
        assert!(app.is_current_fingerprint_approved());
        assert!(!app.can_decide());
        assert_eq!(app.decision, None);
    }

    #[test]
    fn verified_fingerprints_must_cover_exactly_the_displayed_sources() {
        let (_root, mut app) = fixture(&["fn main() {}\n", "// second script\n"]);
        let mut changed = read_sources(&app.session).unwrap();
        changed[0].contents.push_str("// changed\n");
        let mut missing = read_sources(&app.session).unwrap();
        missing.pop();
        let mut renamed = read_sources(&app.session).unwrap();
        renamed[0].path = "other.rs".into();
        for snapshots in [changed, missing, renamed] {
            assert!(
                app.set_baseline(Baseline::CurrentFingerprint(snapshots.into()))
                    .is_err()
            );
            assert!(!app.is_current_fingerprint_approved());
            assert!(app.can_decide());
            assert_eq!(app.decision, None);
        }
    }

    #[test]
    fn mixed_approvals_allow_version_review_and_preserve_other_entries() {
        let (_root, mut app) = fixture(&["fn main() {}"]);
        let name = app.session.package.name.clone();
        let fingerprint = format!("{:064x}+{:016x}#0.0.1", 1, 2);
        app.session
            .config
            .set_approval(&name, ApprovedVersion::Fingerprint(fingerprint.clone()))
            .unwrap();
        app.session
            .config
            .set_approval(&name, ApprovedVersion::SemVer("0.0.1".parse().unwrap()))
            .unwrap();
        app.session.package.approval =
            app.session.config.config().approved[&name].for_review(&app.session.package.version);
        assert!(app.can_decide());
        assert!(app.decide(Decision::PinVersion).unwrap());
        let approvals = &app.session.config.config().approved[&name];
        assert!(approvals.allows_fingerprint(&fingerprint));
        assert!(approvals.allows_version(&"0.0.1".parse().unwrap()));
        assert!(approvals.allows_version(&app.session.package.version));
        app.decide(Decision::Reject).unwrap();
        let approvals = &app.session.config.config().approved[&name];
        assert!(approvals.allows_fingerprint(&fingerprint));
        assert!(approvals.allows_version(&"0.0.1".parse().unwrap()));
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
        assert!(app.decide(Decision::PinVersion).is_err());
        assert_eq!(app.decision, None);
        assert!(app.session.package.approval.is_none());
        assert_eq!(fs::read(&path).unwrap(), before);

        app.session.config = ConfigFile::open(app.session.config.target().clone()).unwrap();
        app.decide(Decision::PinVersion).unwrap();
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
        let error = app.decide(Decision::PinVersion).unwrap_err();
        app.report_decision_error(error);
        assert!(app.notice.as_ref().unwrap().is_error);
        assert_eq!(app.decision, None);
        assert!(app.session.package.approval.is_none());
        assert_eq!(fs::read(app.session.config.path()).unwrap(), before);

        app.session.config = ConfigFile::open(app.session.config.target().clone()).unwrap();
        assert!(app.decide(Decision::PinVersion).unwrap());
        let changed = format!(
            "{}\n# external edit\n",
            fs::read_to_string(app.session.config.path()).unwrap()
        );
        fs::write(app.session.config.path(), &changed).unwrap();
        assert!(app.decide(Decision::Reject).is_err());
        assert_eq!(app.decision, Some(Decision::PinVersion));
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
