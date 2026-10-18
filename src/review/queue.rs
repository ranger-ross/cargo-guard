use std::{collections::VecDeque, path::PathBuf};

use unicode_width::UnicodeWidthStr;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{
    ReviewPackage, ReviewSession,
    app::{App, Decision},
    discovery::ReviewQueue,
    input::{self, Action},
    sources::{Baseline, BaselineLoader},
};
use crate::config::{ApprovedVersion, edit::ConfigFile};

const VIEW_CACHE_CAPACITY: usize = 8;

pub struct QueueState {
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub app: Option<App>,
    config_path: PathBuf,
    loader: Option<BaselineLoader>,
    retired_loaders: Vec<BaselineLoader>,
    cached_apps: VecDeque<(usize, App)>,
    pub sidebar_width: u16,
}

impl QueueState {
    pub fn new(queue: ReviewQueue) -> Self {
        let config_path = queue.config.path().to_path_buf();
        let entries: Vec<_> = queue
            .packages
            .into_iter()
            .map(|package| Entry {
                package,
                decision: None,
                skipped: false,
                error: None,
                baseline_unavailable: false,
                fingerprint_approved: false,
            })
            .collect();
        let selected = entries.iter().position(Entry::actionable).unwrap_or(0);
        let sidebar_width = entries
            .iter()
            .map(|entry| format!("{} v{}", entry.package.name, entry.package.version).width() + 5)
            .max()
            .unwrap_or(24)
            .clamp(24, 36) as u16;
        let mut state = Self {
            entries,
            selected,
            app: None,
            config_path,
            loader: None,
            retired_loaders: Vec::new(),
            cached_apps: VecDeque::new(),
            sidebar_width,
        };
        if !state.entries.is_empty() {
            state.open(selected);
        }
        state
    }

    pub fn poll(&mut self) -> bool {
        self.retired_loaders.retain(|loader| !loader.is_finished());
        if let Some(baseline) = self.loader.as_ref().and_then(BaselineLoader::poll) {
            self.entries[self.selected].baseline_unavailable =
                matches!(baseline, Baseline::Unavailable { .. });
            if let Some(app) = &mut self.app
                && let Err(error) = app.set_baseline(baseline)
            {
                self.entries[self.selected].error =
                    Some(format!("could not display baseline: {error:#}"));
                self.app = None;
            }
            self.entries[self.selected].fingerprint_approved = self
                .app
                .as_ref()
                .is_some_and(App::is_current_fingerprint_approved);
            self.retire_loader();
            return true;
        }
        false
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
                .and_then(|a| a.for_review(&entry.package.version));
        }
        self.entries[self.selected].decision = app.decision;
        self.entries[self.selected].fingerprint_approved = app.is_current_fingerprint_approved();
        self.entries[self.selected].skipped = false;
        self.retire_loader();
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
            KeyCode::Char('j') => Action::NextPackage,
            KeyCode::Char('k') => Action::PreviousPackage,
            KeyCode::Char('s') => Action::Skip,
            _ => Action::Continue,
        }
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for entry in &self.entries {
            match entry.decision {
                Some(Decision::Approve | Decision::ApproveFingerprint) => summary.approved += 1,
                Some(Decision::Reject) => summary.rejected += 1,
                None if entry.error.is_some() => summary.unresolved += 1,
                None if entry.already_approved() => summary.already_approved += 1,
                None if entry.baseline_unavailable => summary.unresolved += 1,
                None => summary.skipped += 1,
            }
        }
        summary
    }

    fn open(&mut self, selected: usize) {
        // Cancellation is requested immediately, but joining a worker (which
        // may be waiting for Cargo) must not block package navigation.
        self.retire_loader();
        if let Some(app) = self.app.take() {
            self.cached_apps.push_back((self.selected, app));
            if self.cached_apps.len() > VIEW_CACHE_CAPACITY {
                self.cached_apps.pop_front();
            }
        }
        self.selected = selected;
        self.entries[selected].error = None;
        self.entries[selected].baseline_unavailable = false;
        self.entries[selected].fingerprint_approved = false;
        if self.entries[selected].decision == Some(Decision::ApproveFingerprint) {
            self.entries[selected].decision = None;
        }
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
            entry.package.approval = config
                .config()
                .approved
                .get(&entry.package.name)
                .and_then(|a| a.for_review(&entry.package.version));
        }
        let entry = &self.entries[self.selected];
        let session = ReviewSession {
            package: entry.package.clone(),
            config,
        };
        let mut app = if let Some(index) = self
            .cached_apps
            .iter()
            .position(|(index, _)| *index == self.selected)
        {
            let (_, mut app) = self.cached_apps.remove(index).expect("cached index exists");
            app.refresh(session)?;
            app
        } else {
            App::new(session)?
        };
        app.queue_mode = true;
        app.decision = entry.decision;
        let loader = if matches!(app.baseline, Baseline::Ready(_) | Baseline::CurrentVersion) {
            None
        } else {
            BaselineLoader::start(&app.session)?
        };
        Ok((app, loader))
    }

    fn retire_loader(&mut self) {
        if let Some(loader) = self.loader.take() {
            loader.cancel();
            self.retired_loaders.push(loader);
        }
        // Drop only completed workers here. Remaining workers are joined on
        // exit, after their cancellation path has killed/reaped Cargo.
        self.retired_loaders.retain(|loader| !loader.is_finished());
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
}

pub struct Entry {
    pub package: ReviewPackage,
    pub decision: Option<Decision>,
    pub skipped: bool,
    pub error: Option<String>,
    pub baseline_unavailable: bool,
    pub fingerprint_approved: bool,
}

impl Entry {
    pub fn status(&self) -> &str {
        if self.error.is_some() {
            "load error"
        } else if matches!(
            self.decision,
            Some(Decision::Approve | Decision::ApproveFingerprint)
        ) {
            "approved"
        } else if self.decision == Some(Decision::Reject) {
            "rejected"
        } else if self.already_approved() {
            "already approved"
        } else if self.fingerprint_only() {
            "fingerprint"
        } else if self.skipped {
            "skipped"
        } else if self.baseline_unavailable {
            "baseline unavailable"
        } else {
            "pending"
        }
    }

    fn already_approved(&self) -> bool {
        self.fingerprint_approved
            || matches!(&self.package.approval, Some(ApprovedVersion::SemVer(version)) if version == &self.package.version)
    }

    fn fingerprint_only(&self) -> bool {
        matches!(self.package.approval, Some(ApprovedVersion::Fingerprint(_)))
    }

    fn actionable(&self) -> bool {
        self.decision.is_none() && !self.skipped && !self.already_approved() && self.error.is_none()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, QueueState) {
        let (root, metadata) = super::super::tests::fixture();
        let queue = ReviewQueue::from_metadata(&metadata).unwrap();
        (root, QueueState::new(queue))
    }

    fn fingerprint_fixture() -> (tempfile::TempDir, QueueState, PathBuf) {
        let (root, state) = fixture();
        let script = &state.app.as_ref().unwrap().session.package.scripts[0];
        let helper = script.path.with_file_name("helper.rs");
        fs::write(&helper, "const INPUT: u8 = 1;\n").unwrap();
        let artifact = root.path().join(format!(
            "target/debug/build/custom-script/out/{}.d",
            script.name.replace('-', "_")
        ));
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(
            &artifact,
            format!("binary: {} {}\n", script.path.display(), helper.display()),
        )
        .unwrap();
        (root, state, artifact)
    }

    fn finish_baseline(state: &mut QueueState) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !state.poll() {
            assert!(std::time::Instant::now() < deadline, "baseline timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn navigation_remains_available_without_sources() {
        let (_root, mut state) = fixture();
        state.app = None;
        for (character, action) in [
            ('j', Action::NextPackage),
            ('k', Action::PreviousPackage),
            ('n', Action::Continue),
            ('p', Action::Continue),
        ] {
            let key = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
            assert_eq!(state.handle_key(key), action);
            assert_eq!(
                state.handle_key(KeyEvent {
                    kind: KeyEventKind::Release,
                    ..key
                }),
                Action::Continue
            );
        }
    }

    #[test]
    fn cached_views_preserve_rows_and_scroll_but_refresh_sources_and_approvals() {
        let (root, mut state) = fixture();
        let rows = state.app.as_ref().unwrap().file().rows.as_ptr();
        state.app.as_mut().unwrap().set_viewport(1, 10);
        state.app.as_mut().unwrap().scroll(1, 0);
        let top = state.app.as_ref().unwrap().top;
        state.navigate(false);
        state.navigate(true);
        assert_eq!(state.app.as_ref().unwrap().file().rows.as_ptr(), rows);
        assert_eq!(state.app.as_ref().unwrap().top, top);

        state.navigate(false);
        fs::write(
            root.path().join("deps/custom/scripts/custom.rs"),
            "fn changed() {}\n",
        )
        .unwrap();
        state.navigate(true);
        let text = state.app.as_ref().unwrap().file().rows[0].text.to_string();
        assert_eq!(text, "fn changed() {}");
        assert_eq!(state.app.as_ref().unwrap().top, 0);

        state.navigate(false);
        fs::write(
            root.path().join("approved-builds.toml"),
            "[approved]\ncustom-script='2.0.0'\n",
        )
        .unwrap();
        state.navigate(true);
        assert!(state.app.as_ref().unwrap().is_current_version_approved());
        assert!(matches!(
            state.app.as_ref().unwrap().baseline,
            Baseline::CurrentVersion
        ));
    }

    #[test]
    fn cached_baselines_do_not_restart_source_retrieval() {
        use crate::review::sources::PackageSources;

        let (_root, mut state) = fixture();
        let app = state.app.as_mut().unwrap();
        let version = "1.0.0".parse::<semver::Version>().unwrap();
        app.session
            .config
            .set_approval("custom-script", ApprovedVersion::SemVer(version.clone()))
            .unwrap();
        app.session.package.approval = Some(ApprovedVersion::SemVer(version.clone()));
        app.set_baseline(Baseline::Ready(PackageSources {
            version,
            scripts: vec![],
        }))
        .unwrap();
        let rows = app.file().rows.as_ptr();
        state.navigate(false);
        state.navigate(true);
        assert!(matches!(
            state.app.as_ref().unwrap().baseline,
            Baseline::Ready(_)
        ));
        assert_eq!(state.app.as_ref().unwrap().file().rows.as_ptr(), rows);
        assert!(state.loader.is_none());
    }

    #[test]
    fn cached_fingerprint_approvals_are_rechecked_after_helper_changes() {
        let (_root, mut state, artifact) = fingerprint_fixture();
        let selected = state.selected;
        let script = state.entries[selected].package.scripts[0].path.clone();
        let displayed = fs::read_to_string(&script).unwrap();
        state.decide(Decision::ApproveFingerprint).unwrap();
        let approval = state.entries[selected].package.approval.clone();
        assert!(state.entries[selected].fingerprint_approved);
        assert_eq!(state.summary().approved, 1);

        state.navigate(true);
        assert_eq!(state.selected, selected);
        assert_eq!(state.entries[selected].decision, None);
        assert!(!state.entries[selected].fingerprint_approved);
        assert!(state.app.as_ref().unwrap().can_decide());
        finish_baseline(&mut state);
        assert!(state.entries[selected].already_approved());
        assert!(!state.entries[selected].actionable());
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert_eq!(state.app.as_ref().unwrap().decision, None);
        assert_eq!(state.summary().already_approved, 1);
        assert_eq!(state.summary().approved, 0);

        state.navigate(false);
        fs::write(script.with_file_name("helper.rs"), "const INPUT: u8 = 2;\n").unwrap();
        // Keep dep-info fresh so the changed input hash, not freshness alone,
        // determines whether the saved approval still covers this package.
        let dep_info = fs::read_to_string(&artifact).unwrap();
        fs::write(&artifact, dep_info).unwrap();
        state.navigate(true);
        assert_eq!(fs::read_to_string(&script).unwrap(), displayed);
        assert_eq!(state.entries[selected].package.approval, approval);
        assert_eq!(state.entries[selected].decision, None);
        assert!(!state.entries[selected].fingerprint_approved);
        assert!(state.app.as_ref().unwrap().can_decide());
        finish_baseline(&mut state);
        assert!(!state.entries[selected].already_approved());
        assert!(state.entries[selected].actionable());
        assert!(state.app.as_ref().unwrap().can_decide());
        assert_eq!(state.summary().already_approved, 0);
        assert_eq!(state.summary().approved, 0);
    }

    #[test]
    fn reopened_fingerprint_queue_updates_approved_status_and_summary() {
        let (_root, mut state, _artifact) = fingerprint_fixture();
        state.decide(Decision::ApproveFingerprint).unwrap();
        let queue = ReviewQueue {
            packages: state
                .entries
                .iter()
                .map(|entry| entry.package.clone())
                .collect(),
            config: ConfigFile::open(&state.config_path).unwrap(),
        };
        let mut reopened = QueueState::new(queue);
        assert_eq!(reopened.selected, 0);
        assert!(reopened.entries[0].actionable());
        finish_baseline(&mut reopened);
        assert!(reopened.entries[0].fingerprint_only());
        assert!(reopened.entries[0].already_approved());
        assert_eq!(reopened.entries[0].status(), "already approved");
        assert!(!reopened.entries[0].actionable());
        assert_eq!(reopened.entries[0].decision, None);
        assert!(!reopened.app.as_ref().unwrap().can_decide());
        assert_eq!(reopened.app.as_ref().unwrap().decision, None);
        assert_eq!(
            reopened.summary(),
            Summary {
                already_approved: 1,
                skipped: 3,
                ..Default::default()
            }
        );
    }

    #[test]
    fn cached_view_keeps_save_conflict_detection() {
        let (root, mut state) = fixture();
        state.navigate(false);
        state.navigate(true);
        let external = "[approved]\nother='1.0.0'\n";
        fs::write(root.path().join("approved-builds.toml"), external).unwrap();
        assert!(state.decide(Decision::Approve).is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("approved-builds.toml")).unwrap(),
            external
        );
    }

    #[test]
    fn view_cache_is_bounded() {
        let (_root, metadata) = super::super::tests::fixture();
        let mut queue = ReviewQueue::from_metadata(&metadata).unwrap();
        let template = queue.packages[0].clone();
        for index in 0..VIEW_CACHE_CAPACITY + 4 {
            let mut package = template.clone();
            package.name = format!("cache-fixture-{index}");
            package.id.repr = format!("path+file:///cache-fixture-{index}#1.0.0");
            queue.packages.push(package);
        }
        let mut state = QueueState::new(queue);
        for _ in 0..state.entries.len() * 2 {
            state.navigate(false);
            assert!(state.cached_apps.len() <= VIEW_CACHE_CAPACITY);
        }
    }

    #[test]
    #[ignore = "manual navigation performance measurement"]
    fn navigation_benchmark() {
        let (root, metadata) = super::super::tests::fixture();
        let code: String = (0..200).map(|i| format!("fn helper_{i}() {{ let message = \"build script input\"; println!(\"{{message}}\"); }}\n")).collect();
        let queue = ReviewQueue::from_metadata(&metadata).unwrap();
        for package in &queue.packages {
            for script in &package.scripts {
                fs::write(&script.path, &code).unwrap();
            }
        }
        let cold_start = std::time::Instant::now();
        let mut state = QueueState::new(queue);
        for _ in 0..state.entries.len() {
            state.navigate(false);
        }
        println!(
            "First pass through {} packages: {:?}",
            state.entries.len(),
            cold_start.elapsed()
        );
        let start = std::time::Instant::now();
        for _ in 0..40 {
            state.navigate(false);
        }
        let elapsed = start.elapsed();
        println!(
            "40 package switches: {elapsed:?} ({:?}/switch)",
            elapsed / 40
        );
        std::hint::black_box(&state.app);
        drop(root);
    }

    #[test]
    fn partial_completion_survives_reopening_the_queue() {
        let (root, mut state) = fixture();
        state.decide(Decision::Approve).unwrap();
        assert_eq!(
            state.summary(),
            Summary {
                approved: 1,
                skipped: 3,
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
                skipped: 3,
                ..Default::default()
            }
        );
    }

    #[test]
    fn fingerprint_entries_are_actionable_and_version_approvals_are_read_only() {
        let (root, metadata) = super::super::tests::fixture();
        fs::write(
            root.path().join("approved-builds.toml"),
            "[approved]\ncustom-script='2.0.0'\ndefault-script='fingerprint'\nduplicate=['1.0.0','2.0.0']\n",
        )
        .unwrap();
        let mut state = QueueState::new(ReviewQueue::from_metadata(&metadata).unwrap());
        assert_eq!(state.selected, 1);
        assert_eq!(state.entries[state.selected].status(), "fingerprint");
        assert!(state.app.as_ref().unwrap().can_decide());
        state.navigate(true);
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert_eq!(
            state.summary(),
            Summary {
                already_approved: 3,
                skipped: 1,
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
        assert!(!state.decide(Decision::Approve).unwrap());
        assert_eq!(state.entries[state.selected].package.name, "duplicate");
        assert!(!state.decide(Decision::Approve).unwrap());
        assert!(state.decide(Decision::Approve).unwrap());
        let saved =
            crate::config::load_config_from(&root.path().join("approved-builds.toml")).unwrap();
        assert_eq!(saved.approved.len(), 3);
        assert!(saved.approved["duplicate"].allows_version(&"1.0.0".parse().unwrap()));
        assert!(saved.approved["duplicate"].allows_version(&"2.0.0".parse().unwrap()));
        assert_eq!(
            state.summary(),
            Summary {
                approved: 4,
                ..Default::default()
            }
        );
    }

    #[test]
    fn reject_skip_and_quit_leave_other_packages_pending() {
        let (root, mut state) = fixture();
        assert!(!state.decide(Decision::Reject).unwrap());
        assert!(!state.skip());
        assert!(!root.path().join("approved-builds.toml").exists());
        assert_eq!(
            state.summary(),
            Summary {
                rejected: 1,
                skipped: 3,
                ..Default::default()
            }
        );
    }

    #[test]
    fn approved_packages_are_read_only_but_other_versions_are_actionable() {
        let (_root, mut state) = fixture();
        state.decide(Decision::Approve).unwrap();
        state.navigate(true);
        assert!(!state.app.as_ref().unwrap().can_decide());
        state.navigate(true);
        assert_eq!(state.entries[state.selected].package.name, "duplicate");
        assert!(state.app.as_ref().unwrap().can_decide());
        assert!(state.decide(Decision::Approve).is_ok());
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
        assert_eq!(state.summary().unresolved, 1);
        state.navigate(true);
        assert!(state.app.is_some());
    }
}
