use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{
    ReviewPackage, ReviewSession,
    app::{App, Decision},
    discovery::ReviewQueue,
    input::{self, Action},
    sources::{Baseline, BaselineLoader},
};
use crate::config::{ApprovedVersion, edit::ConfigFile};

pub struct Entry {
    pub package: ReviewPackage,
    pub conflict: bool,
    pub decision: Option<Decision>,
    pub skipped: bool,
    pub error: Option<String>,
    pub baseline_unavailable: bool,
}

impl Entry {
    fn already_approved(&self) -> bool {
        matches!(&self.package.approval, Some(ApprovedVersion::SemVer(version)) if version == &self.package.version)
    }

    fn unsupported(&self) -> bool {
        matches!(self.package.approval, Some(ApprovedVersion::Fingerprint(_)))
    }

    fn actionable(&self) -> bool {
        self.decision.is_none()
            && !self.skipped
            && !self.conflict
            && !self.unsupported()
            && !self.already_approved()
            && self.error.is_none()
    }

    pub fn status(&self) -> &str {
        if self.conflict {
            "conflict"
        } else if self.error.is_some() {
            "load error"
        } else if self.decision == Some(Decision::Approve) {
            "approved"
        } else if self.decision == Some(Decision::Reject) {
            "rejected"
        } else if self.unsupported() {
            "fingerprint"
        } else if self.already_approved() {
            "already approved"
        } else if self.skipped {
            "skipped"
        } else if self.baseline_unavailable {
            "baseline unavailable"
        } else {
            "pending"
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub approved: usize,
    pub rejected: usize,
    pub skipped: usize,
    pub unresolved: usize,
    pub already_approved: usize,
}

impl Summary {
    pub fn print(&self) {
        println!(
            "Review summary: {} approved, {} rejected, {} skipped, {} unresolved, {} already approved.",
            self.approved, self.rejected, self.skipped, self.unresolved, self.already_approved
        );
    }
}

pub struct QueueState {
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub app: Option<App>,
    config_path: PathBuf,
    loader: Option<BaselineLoader>,
}

impl QueueState {
    pub fn new(queue: ReviewQueue) -> Self {
        let config_path = queue.config.path().to_path_buf();
        let entries: Vec<_> = queue
            .packages
            .into_iter()
            .map(|package| Entry {
                conflict: queue.conflicts.contains_key(&package.name),
                package,
                decision: None,
                skipped: false,
                error: None,
                baseline_unavailable: false,
            })
            .collect();
        let selected = entries.iter().position(Entry::actionable).unwrap_or(0);
        let mut state = Self {
            entries,
            selected,
            app: None,
            config_path,
            loader: None,
        };
        if !state.entries.is_empty() {
            state.open(selected);
        }
        state
    }

    fn open(&mut self, selected: usize) {
        // Dropping cancels and joins the previous loader, so a late result can
        // never be attached to a different package.
        self.loader = None;
        self.app = None;
        self.selected = selected;
        self.entries[selected].error = None;
        self.entries[selected].baseline_unavailable = false;
        match self.load_current() {
            Ok((app, loader)) => {
                self.app = Some(app);
                self.loader = loader;
            }
            Err(error) => self.entries[selected].error = Some(format!("{error:#}")),
        }
    }

    fn load_current(&mut self) -> eyre::Result<(App, Option<BaselineLoader>)> {
        // Each newly selected package gets a fresh edit snapshot. This includes
        // our previous saves while preserving conflict detection during review.
        let config = ConfigFile::open(&self.config_path)?;
        for entry in &mut self.entries {
            entry.package.approval = config.config().approved.get(&entry.package.name).cloned();
        }
        let entry = &self.entries[self.selected];
        let mut app = App::new(ReviewSession {
            package: entry.package.clone(),
            config,
        })?;
        app.queue_mode = true;
        app.decision = entry.decision;
        if entry.conflict {
            app.decision_blocked = Some("Multiple versions/sources share this name; one approval entry cannot represent all of them".into());
        }
        let loader = if entry.conflict {
            None
        } else {
            BaselineLoader::start(&app.session)?
        };
        if entry.conflict && matches!(app.baseline, Baseline::Loading(_)) {
            app.set_baseline(Baseline::Unavailable {
                version: match &entry.package.approval {
                    Some(ApprovedVersion::SemVer(version)) => version.clone(),
                    _ => unreachable!(),
                },
                reason: "ambiguous package name: historical source selection is unresolved".into(),
            })?;
        }
        Ok((app, loader))
    }

    pub fn poll(&mut self) {
        if let Some(baseline) = self.loader.as_ref().and_then(BaselineLoader::poll) {
            self.entries[self.selected].baseline_unavailable =
                matches!(baseline, Baseline::Unavailable { .. });
            if let Some(app) = &mut self.app {
                if let Err(error) = app.set_baseline(baseline) {
                    self.entries[self.selected].error =
                        Some(format!("could not display baseline: {error:#}"));
                    self.app = None;
                }
            }
            self.loader = None;
        }
    }

    pub fn navigate(&mut self, backwards: bool) {
        let len = self.entries.len();
        let selected = if backwards {
            (self.selected + len - 1) % len
        } else {
            (self.selected + 1) % len
        };
        self.open(selected);
    }

    /// Returns true when there are no remaining undecided actionable packages.
    fn advance(&mut self) -> bool {
        let len = self.entries.len();
        for offset in 1..=len {
            let next = (self.selected + offset) % len;
            if self.entries[next].actionable() {
                self.open(next);
                return false;
            }
        }
        true
    }

    pub fn skip(&mut self) -> bool {
        self.entries[self.selected].skipped = true;
        self.advance()
    }

    pub fn decide(&mut self, decision: Decision) -> eyre::Result<bool> {
        let app = self
            .app
            .as_mut()
            .ok_or_else(|| eyre::eyre!("current package sources could not be loaded"))?;
        eyre::ensure!(app.can_decide(), "this package is read-only");
        app.decide(decision)?;
        for entry in &mut self.entries {
            entry.package.approval = app
                .session
                .config
                .config()
                .approved
                .get(&entry.package.name)
                .cloned();
        }
        self.entries[self.selected].decision = Some(decision);
        self.entries[self.selected].skipped = false;
        self.loader = None;
        Ok(self.advance())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if let Some(app) = &mut self.app {
            return input::handle_key(app, key);
        }
        if key.kind == KeyEventKind::Release {
            return Action::Continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('n') => Action::NextPackage,
            KeyCode::Char('p') => Action::PreviousPackage,
            KeyCode::Char('s') => Action::Skip,
            _ => Action::Continue,
        }
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for entry in &self.entries {
            match entry.decision {
                Some(Decision::Approve) => summary.approved += 1,
                Some(Decision::Reject) => summary.rejected += 1,
                None if entry.conflict || entry.unsupported() || entry.error.is_some() => {
                    summary.unresolved += 1
                }
                None if entry.already_approved() => summary.already_approved += 1,
                None if entry.baseline_unavailable => summary.unresolved += 1,
                None => summary.skipped += 1,
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, QueueState) {
        let (root, metadata) = super::super::tests::fixture();
        let queue = ReviewQueue::from_metadata(&metadata).unwrap();
        (root, QueueState::new(queue))
    }

    #[test]
    fn partial_completion_survives_reopening_the_queue() {
        let (root, mut state) = fixture();
        state.decide(Decision::Approve).unwrap();
        assert_eq!(
            state.summary(),
            Summary {
                approved: 1,
                skipped: 1,
                unresolved: 2,
                ..Default::default()
            }
        );
        drop(state);
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(root.path().join("Cargo.toml"))
            .exec()
            .unwrap();
        let reopened = QueueState::new(ReviewQueue::from_metadata(&metadata).unwrap());
        assert_eq!(
            reopened.entries[reopened.selected].package.name,
            "default-script"
        );
        assert_eq!(
            reopened.summary(),
            Summary {
                already_approved: 1,
                skipped: 1,
                unresolved: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn queue_with_only_read_only_entries_opens_for_inspection() {
        let (root, metadata) = super::super::tests::fixture();
        fs::write(
            root.path().join("approved-builds.toml"),
            "[approved]\ncustom-script='2.0.0'\ndefault-script='fingerprint'\n",
        )
        .unwrap();
        let mut state = QueueState::new(ReviewQueue::from_metadata(&metadata).unwrap());
        assert_eq!(state.selected, 0);
        assert!(!state.app.as_ref().unwrap().can_decide());
        state.navigate(false);
        assert_eq!(state.entries[state.selected].status(), "fingerprint");
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert_eq!(
            state.summary(),
            Summary {
                already_approved: 1,
                unresolved: 3,
                ..Default::default()
            }
        );
    }

    #[test]
    fn decisions_advance_and_preserve_previous_saves() {
        let (root, mut state) = fixture();
        assert_eq!(state.entries[state.selected].package.name, "custom-script");
        assert!(!state.decide(Decision::Approve).unwrap());
        assert_eq!(state.entries[state.selected].package.name, "default-script");
        assert!(state.decide(Decision::Approve).unwrap());
        let saved =
            crate::config::load_config_from(&root.path().join("approved-builds.toml")).unwrap();
        assert_eq!(saved.approved.len(), 2);
        assert_eq!(
            state.summary(),
            Summary {
                approved: 2,
                unresolved: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn reject_skip_and_quit_leave_other_packages_pending() {
        let (root, mut state) = fixture();
        assert!(!state.decide(Decision::Reject).unwrap());
        assert!(state.skip());
        assert!(!root.path().join("approved-builds.toml").exists());
        assert_eq!(
            state.summary(),
            Summary {
                rejected: 1,
                skipped: 1,
                unresolved: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn approved_packages_and_conflicts_remain_browsable_and_read_only() {
        let (_root, mut state) = fixture();
        state.decide(Decision::Approve).unwrap();
        state.navigate(true);
        assert!(!state.app.as_ref().unwrap().can_decide());
        state.navigate(true);
        assert!(state.entries[state.selected].conflict);
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert!(state.decide(Decision::Approve).is_err());
    }

    #[test]
    fn conflicts_on_save_do_not_advance_or_change_decisions() {
        let (root, mut state) = fixture();
        let external = "[approved]\nother='1.0.0'\n";
        fs::write(root.path().join("approved-builds.toml"), external).unwrap();
        let index = state.selected;
        assert!(state.decide(Decision::Approve).is_err());
        assert_eq!(state.selected, index);
        assert_eq!(state.entries[index].decision, None);
        assert_eq!(
            fs::read_to_string(root.path().join("approved-builds.toml")).unwrap(),
            external
        );
    }

    #[test]
    fn missing_source_is_unresolved_and_navigation_still_works() {
        let (root, mut state) = fixture();
        fs::remove_file(root.path().join("deps/default/build.rs")).unwrap();
        state.navigate(false);
        assert!(state.app.is_none());
        assert!(state.entries[state.selected].error.is_some());
        assert_eq!(state.summary().unresolved, 3);
        state.navigate(true);
        assert!(state.app.is_some());
    }
}
