use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use unicode_width::UnicodeWidthStr;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use super::{
    ReviewPackage, ReviewSession,
    app::{App, Decision},
    diagnostic::Diagnostic,
    discovery::ReviewQueue,
    fingerprint::Verification,
    input::{self, Action},
    sources::{Baseline, BaselineLoader},
    verification::{self, Scope, Verifier},
};
use crate::config::{ApprovalTarget, ApprovedVersion, edit::ConfigFile};

const VIEW_CACHE_CAPACITY: usize = 8;
const RECHECK_INTERVAL: Duration = Duration::from_secs(1);

pub struct QueueState {
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub app: Option<App>,
    pub error_diagnostic: Diagnostic,
    pub show_failure_help: bool,
    pub failure_help: Diagnostic,
    pub approval_target: ApprovalTarget,
    global_target: Option<ApprovalTarget>,
    pub workspace_root: PathBuf,
    pub policy_notes: Vec<String>,
    loader: Option<BaselineLoader>,
    retired_loaders: Vec<BaselineLoader>,
    cached_apps: VecDeque<(usize, App)>,
    pub sidebar_width: u16,
    pub package_rows: Rect,
    pub package_start: usize,
    package_pointer: Option<Position>,
    verifier: Result<Verifier, String>,
    last_refresh: Instant,
}

impl QueueState {
    pub fn new(queue: ReviewQueue) -> Self {
        let approval_target = queue.config.target().clone();
        let entries: Vec<_> = queue
            .packages
            .into_iter()
            .map(|mut package| {
                let approvals = queue.config.config().approved.get(&package.name).cloned();
                package.approval = approvals
                    .as_ref()
                    .and_then(|approval| approval.for_review(&package.version));
                Entry {
                    package,
                    approvals,
                    decision: None,
                    skipped: false,
                    error: None,
                    baseline_unavailable: false,
                    verification: None,
                    generation: 0,
                    checking: false,
                    global_approvals: None,
                    global_verification: None,
                    global_generation: 0,
                    global_checking: false,
                }
            })
            .collect();
        let selected = entries.iter().position(Entry::actionable).unwrap_or(0);
        let sidebar_width = entries
            .iter()
            .map(|entry| format!("{} v{}", entry.package.name, entry.package.version).width() + 5)
            .max()
            .unwrap_or(24)
            .clamp(24, 36) as u16;
        let verifier = Verifier::start(queue.workspace_root.clone(), queue.build_directory)
            .map_err(|error| format!("{error:#}"));
        let mut state = Self {
            entries,
            selected,
            error_diagnostic: Diagnostic::default(),
            show_failure_help: false,
            failure_help: Diagnostic::default(),
            app: None,
            approval_target,
            global_target: queue.global_target,
            workspace_root: queue.workspace_root,
            policy_notes: queue.policy_notes,
            loader: None,
            retired_loaders: Vec::new(),
            cached_apps: VecDeque::new(),
            sidebar_width,
            package_rows: Rect::default(),
            package_start: 0,
            package_pointer: None,
            verifier,
            last_refresh: Instant::now(),
        };
        if !state.entries.is_empty() {
            state.open(selected);
        }
        state.schedule_all();
        state
    }

    pub fn poll(&mut self) -> bool {
        self.retired_loaders.retain(|loader| !loader.is_finished());
        let mut dirty = false;
        if self.last_refresh.elapsed() >= RECHECK_INTERVAL {
            self.last_refresh = Instant::now();
            self.refresh_global_approvals();
            match ConfigFile::open(self.approval_target.clone()) {
                Ok(config) => {
                    let generation = self
                        .entries
                        .get(self.selected)
                        .map(|entry| entry.generation);
                    self.sync_approvals(&config);
                    if generation
                        != self
                            .entries
                            .get(self.selected)
                            .map(|entry| entry.generation)
                    {
                        self.open(self.selected);
                    }
                }
                Err(error) => {
                    for entry in &mut self.entries {
                        if entry.needs_verification() {
                            entry.invalidate();
                            entry.verification = Some(Verification::Unavailable(format!(
                                "could not read approvals: {error:#}"
                            )));
                        }
                    }
                    self.update_current();
                    return true;
                }
            }
            self.update_current();
            dirty = true;
            self.schedule_all();
        }
        while let Some(result) = self.verifier.as_ref().ok().and_then(Verifier::poll) {
            dirty |= self.apply_result(result);
        }
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
            self.retire_loader();
            dirty = true;
        }
        dirty
    }

    fn schedule_all(&mut self) {
        for index in 0..self.entries.len() {
            self.schedule(index);
        }
        if let Ok(verifier) = &self.verifier {
            verifier.prioritize(self.selected);
        }
    }

    fn schedule(&mut self, index: usize) {
        self.schedule_scope(index, Scope::Selected);
        self.schedule_scope(index, Scope::Global);
    }

    fn schedule_scope(&mut self, index: usize, scope: Scope) {
        let entry = &mut self.entries[index];
        let (checking, needed, generation, approvals) = match scope {
            Scope::Selected => (
                entry.checking,
                entry.needs_verification(),
                entry.generation,
                &entry.approvals,
            ),
            Scope::Global => (
                entry.global_checking,
                entry.needs_global_verification(),
                entry.global_generation,
                &entry.global_approvals,
            ),
        };
        if checking || !needed {
            return;
        }
        let request = verification::Request {
            index,
            scope,
            generation,
            package: entry.package.clone(),
            approvals: approvals.clone().expect("saved fingerprint approval"),
        };
        let result = match &self.verifier {
            Ok(verifier) => verifier
                .verify(request)
                .map_err(|error| format!("{error:#}")),
            Err(error) => Err(error.clone()),
        };
        match result {
            Ok(()) => match scope {
                Scope::Selected => entry.checking = true,
                Scope::Global => entry.global_checking = true,
            },
            Err(error) => {
                let verification = match scope {
                    Scope::Selected => &mut entry.verification,
                    Scope::Global => &mut entry.global_verification,
                };
                *verification = Some(Verification::Unavailable(error));
                if index == self.selected {
                    self.update_current();
                }
            }
        }
    }

    fn apply_result(&mut self, result: verification::Result) -> bool {
        let entry = &mut self.entries[result.index];
        if result.scope == Scope::Global {
            if result.generation != entry.global_generation
                || result.package_id != entry.package.id
                || !entry.needs_global_verification()
            {
                return false;
            }
            entry.global_checking = false;
            let changed = entry.global_verification.as_ref() != Some(&result.verification);
            entry.global_verification = Some(result.verification);
            if result.index == self.selected {
                self.update_current();
            }
            return changed;
        }
        if result.generation != entry.generation
            || result.package_id != entry.package.id
            || !entry.needs_verification()
        {
            return false;
        }
        entry.checking = false;
        if entry.verification.as_ref() == Some(&result.verification) {
            return false;
        }
        if !matches!(result.verification, Verification::Verified(_))
            && entry.decision == Some(Decision::ApproveFingerprint)
        {
            entry.decision = None;
        }
        entry.verification = Some(result.verification);
        if result.index == self.selected {
            self.update_current();
        }
        true
    }

    fn refresh_global_approvals(&mut self) -> bool {
        // This store is informational: unreadable or malformed content clears
        // its indicators but must never prevent editing workspace approvals.
        let config = self
            .global_target
            .as_ref()
            .and_then(|target| ConfigFile::open(target.clone()).ok());
        let mut changed = false;
        for entry in &mut self.entries {
            let approvals = config
                .as_ref()
                .and_then(|config| config.config().approved.get(&entry.package.name));
            if entry.global_approvals.as_ref() != approvals {
                entry.invalidate_global();
                entry.global_approvals = approvals.cloned();
                changed = true;
            }
        }
        changed
    }

    fn sync_approvals(&mut self, config: &ConfigFile) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            let approvals = config.config().approved.get(&entry.package.name);
            if entry.approvals.as_ref() != approvals {
                entry.invalidate();
                entry.approvals = approvals.cloned();
                entry.package.approval =
                    approvals.and_then(|approval| approval.for_review(&entry.package.version));
                if entry
                    .package
                    .approval
                    .as_ref()
                    .is_some_and(ApprovedVersion::is_blanket)
                    || (entry.decision == Some(Decision::PinVersion)
                        && !approvals.is_some_and(|approval| {
                            approval.allows_version(&entry.package.version)
                        }))
                {
                    entry.decision = None;
                }
                changed = true;
            }
        }
        changed
    }

    fn update_current(&mut self) {
        let Some(mut app) = self.app.take() else {
            return;
        };
        let entry = &mut self.entries[self.selected];
        let result = app
            .refresh_sources()
            .and_then(|_| Self::present_verification(entry, &mut app));
        match result {
            Ok(()) => {
                self.app = Some(app);
                if self.entries[self.selected].already_approved() {
                    self.retire_loader();
                }
                self.start_baseline();
                let entry = &self.entries[self.selected];
                if (entry.needs_verification() && entry.verification.is_none())
                    || (entry.needs_global_verification() && entry.global_verification.is_none())
                {
                    self.schedule(self.selected);
                }
            }
            Err(error) => {
                entry.invalidate_global();
                entry.error = Some(format!("could not refresh sources: {error:#}"));
            }
        }
    }

    fn present_verification(entry: &mut Entry, app: &mut App) -> eyre::Result<()> {
        if let Some(Verification::Verified(sources)) = &entry.verification
            && sources != &app.sources()
        {
            entry.invalidate();
        }
        if let Some(Verification::Verified(sources)) = &entry.global_verification
            && sources != &app.sources()
        {
            entry.invalidate_global();
        }
        app.globally_approved = entry.globally_approved();
        app.decision = entry.decision;
        app.verification_pending = entry.needs_verification() && entry.verification.is_none();
        match &entry.verification {
            Some(Verification::Verified(sources)) => {
                app.set_baseline(Baseline::CurrentFingerprint(Arc::clone(sources)))?;
                app.verification_message = None;
            }
            result => {
                if app.is_current_fingerprint_approved() {
                    app.decision = None;
                    app.set_baseline(Baseline::for_session(&app.session))?;
                }
                app.verification_message = if entry.needs_verification() {
                    Some(match result {
                        Some(Verification::Mismatch) => {
                            "Current build-script inputs differ from saved fingerprint approvals"
                                .into()
                        }
                        Some(Verification::Unavailable(reason)) => {
                            format!("Cannot verify current fingerprints: {reason}")
                        }
                        None => "Checking current fingerprints against saved approvals".into(),
                        Some(Verification::Verified(_)) => unreachable!(),
                    })
                } else {
                    None
                };
            }
        }
        if !app.can_decide() {
            app.pin_dialog = None;
        }
        Ok(())
    }

    fn start_baseline(&mut self) {
        let entry = &self.entries[self.selected];
        if self.loader.is_some()
            || (entry.needs_verification()
                && matches!(entry.verification, None | Some(Verification::Verified(_))))
        {
            return;
        }
        if let Some(app) = &self.app
            && !matches!(
                app.baseline,
                Baseline::Ready(_)
                    | Baseline::Blanket
                    | Baseline::CurrentVersion
                    | Baseline::CurrentFingerprint(_)
            )
        {
            match BaselineLoader::start(&app.session) {
                Ok(loader) => self.loader = loader,
                Err(error) => self.entries[self.selected].error = Some(format!("{error:#}")),
            }
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
        if !app.decide(decision)? {
            return Ok(false);
        }
        let decision = app.decision;
        let verified = (decision == Some(Decision::ApproveFingerprint)).then(|| app.sources());
        let config = ConfigFile::open(self.approval_target.clone())?;
        self.sync_approvals(&config);
        let entry = &mut self.entries[self.selected];
        entry.generation += 1;
        entry.checking = false;
        entry.decision = decision;
        entry.verification = verified.map(Verification::Verified);
        entry.skipped = false;
        self.retire_loader();
        self.schedule_all();
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
        if key.code == KeyCode::Char('?') && key.kind == KeyEventKind::Press {
            self.show_failure_help = !self.show_failure_help;
            self.failure_help.reset_view();
            self.error_diagnostic.selection.clear();
            return Action::Continue;
        }
        if self.show_failure_help {
            return input::handle_help_key(
                &mut self.show_failure_help,
                &mut self.failure_help,
                key,
            );
        }
        if key.code == KeyCode::Char('y') && self.error_diagnostic.selection.is_active() {
            return Action::CopySelection;
        }
        match key.code {
            KeyCode::Esc if self.error_diagnostic.selection.clear() => Action::Continue,
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('j') => Action::NextPackage,
            KeyCode::Char('k') => Action::PreviousPackage,
            KeyCode::Char('s') => Action::Skip,
            _ => Action::Continue,
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> (bool, Action) {
        if let Some(dialog) = self.app.as_mut().and_then(|app| app.pin_dialog.as_mut()) {
            return dialog.handle_mouse(event);
        }
        let previous_hover = self.hovered_package();
        self.package_pointer = (!event.modifiers.contains(KeyModifiers::SHIFT)
            && !matches!(event.kind, MouseEventKind::Drag(_)))
        .then_some(Position::new(event.column, event.row));
        let hover_changed = previous_hover != self.hovered_package();
        if event.modifiers.contains(KeyModifiers::SHIFT)
            || self.app.as_ref().is_some_and(|app| app.show_help)
            || (self.app.is_none() && self.show_failure_help)
        {
            return (hover_changed, Action::Continue);
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(selected) = self.hovered_package()
            && selected != self.selected
        {
            self.open(selected);
            return (true, Action::Continue);
        }
        let source_changed = if let Some(app) = &mut self.app {
            app.handle_mouse(event)
        } else {
            self.error_diagnostic.handle_mouse(event).unwrap_or(false)
        };
        (hover_changed || source_changed, Action::Continue)
    }

    pub fn hovered_package(&self) -> Option<usize> {
        if (self.app.is_none() && self.show_failure_help)
            || self
                .app
                .as_ref()
                .is_some_and(|app| app.show_help || app.pin_dialog.is_some())
        {
            return None;
        }
        let pointer = self.package_pointer?;
        if !self.package_rows.contains(pointer) {
            return None;
        }
        let index = self.package_start + usize::from(pointer.y - self.package_rows.y);
        (index < self.entries.len()).then_some(index)
    }

    pub fn selected_text(&self) -> Option<String> {
        if let Some(app) = &self.app {
            if let Some(dialog) = &app.pin_dialog {
                return dialog.diagnostic.selected_text();
            }
            app.diagnostic
                .selected_text()
                .or_else(|| app.selection.text(&app.file().rows))
        } else {
            self.error_diagnostic.selected_text()
        }
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for entry in &self.entries {
            match entry.decision {
                Some(Decision::PinVersion | Decision::ApproveFingerprint) => summary.approved += 1,
                Some(Decision::Reject) => summary.rejected += 1,
                None if entry.error.is_some() => summary.unresolved += 1,
                None if entry.already_approved() => summary.already_approved += 1,
                None if entry.baseline_unavailable
                    || matches!(entry.verification, Some(Verification::Unavailable(_))) =>
                {
                    summary.unresolved += 1
                }
                None => summary.skipped += 1,
            }
        }
        summary
    }

    fn open(&mut self, selected: usize) {
        self.error_diagnostic.reset_view();
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
        match self.load_current() {
            Ok(app) => {
                self.app = Some(app);
                self.start_baseline();
                self.schedule(selected);
                if let Ok(verifier) = &self.verifier {
                    verifier.prioritize(selected);
                }
            }
            Err(error) => {
                self.entries[selected].invalidate_global();
                self.entries[selected].error = Some(format!("{error:#}"));
            }
        }
    }

    fn load_current(&mut self) -> eyre::Result<App> {
        // Each newly selected package gets a fresh edit snapshot. This includes
        // our previous saves while preserving conflict detection during review.
        let config = ConfigFile::open(self.approval_target.clone())?;
        self.sync_approvals(&config);
        self.refresh_global_approvals();
        let entry = &self.entries[self.selected];
        let session = ReviewSession {
            package: entry.package.clone(),
            config,
            workspace_root: self.workspace_root.clone(),
            policy_notes: self.policy_notes.clone(),
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
        Self::present_verification(&mut self.entries[self.selected], &mut app)?;
        Ok(app)
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
    approvals: Option<ApprovedVersion>,
    verification: Option<Verification>,
    generation: u64,
    checking: bool,
    global_approvals: Option<ApprovedVersion>,
    global_verification: Option<Verification>,
    global_generation: u64,
    global_checking: bool,
}

impl Entry {
    pub fn status(&self) -> &str {
        if self.error.is_some() {
            "load error"
        } else if matches!(
            self.decision,
            Some(Decision::PinVersion | Decision::ApproveFingerprint)
        ) {
            "approved"
        } else if self.decision == Some(Decision::Reject) {
            "rejected"
        } else if self.already_approved() {
            "already approved"
        } else if self.globally_approved() {
            "approved globally"
        } else if matches!(self.verification, Some(Verification::Unavailable(_))) {
            "verification unavailable"
        } else if matches!(self.verification, Some(Verification::Mismatch)) {
            "fingerprint changed"
        } else if self.needs_verification() {
            "checking"
        } else if self.skipped {
            "skipped"
        } else if self.baseline_unavailable {
            "baseline unavailable"
        } else {
            "pending"
        }
    }

    pub fn globally_approved(&self) -> bool {
        self.global_approvals
            .as_ref()
            .is_some_and(|approval| approval.allows_version(&self.package.version))
            || matches!(self.global_verification, Some(Verification::Verified(_)))
    }

    fn needs_global_verification(&self) -> bool {
        self.global_approvals.as_ref().is_some_and(|approval| {
            !approval.allows_version(&self.package.version)
                && approval
                    .entries()
                    .iter()
                    .any(|entry| matches!(entry, ApprovedVersion::Fingerprint(_)))
        })
    }

    fn invalidate_global(&mut self) {
        self.global_generation += 1;
        self.global_checking = false;
        self.global_verification = None;
    }

    fn already_approved(&self) -> bool {
        matches!(self.verification, Some(Verification::Verified(_)))
            || self
                .approvals
                .as_ref()
                .is_some_and(|approval| approval.allows_version(&self.package.version))
    }

    fn needs_verification(&self) -> bool {
        self.decision != Some(Decision::Reject)
            && !self
                .approvals
                .as_ref()
                .is_some_and(|approval| approval.allows_version(&self.package.version))
            && self.approvals.as_ref().is_some_and(|approval| {
                approval
                    .entries()
                    .iter()
                    .any(|entry| matches!(entry, ApprovedVersion::Fingerprint(_)))
            })
    }

    fn invalidate(&mut self) {
        self.generation += 1;
        self.checking = false;
        self.verification = None;
        self.baseline_unavailable = false;
        if self.decision == Some(Decision::ApproveFingerprint) {
            self.decision = None;
        }
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
        let queue =
            ReviewQueue::from_metadata(&metadata, &crate::args::ReviewOptions::default(), None)
                .unwrap();
        (root, QueueState::new(queue))
    }

    #[test]
    fn failed_fingerprint_does_not_advance_or_record_approval() {
        let (root, mut state) = fixture();
        let selected = state.selected;
        assert!(!state.decide(Decision::ApproveFingerprint).unwrap());
        assert_eq!(state.selected, selected);
        assert!(state.app.as_ref().unwrap().pin_dialog.is_some());
        assert_eq!(state.entries[selected].decision, None);
        assert_eq!(state.summary().approved, 0);
        assert!(!root.path().join("approved-builds.toml").exists());
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

    fn wait_for(state: &mut QueueState, ready: impl Fn(&QueueState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready(state) {
            state.poll();
            assert!(Instant::now() < deadline, "verification timed out");
            std::thread::park_timeout(Duration::from_millis(5));
        }
    }

    fn reopen(state: &QueueState) -> QueueState {
        QueueState::new(ReviewQueue {
            packages: state
                .entries
                .iter()
                .map(|entry| entry.package.clone())
                .collect(),
            config: ConfigFile::open(state.approval_target.clone()).unwrap(),
            global_target: state.global_target.clone(),
            build_directory: state.workspace_root.join("target"),
            workspace_root: state.workspace_root.clone(),
            policy_notes: state.policy_notes.clone(),
        })
    }

    fn with_global_approvals(state: &mut QueueState, approvals: &[ApprovedVersion]) -> PathBuf {
        let target = ApprovalTarget {
            path: state.workspace_root.join("global-approvals.toml"),
            scope: crate::config::ApprovalScope::Global,
        };
        ConfigFile::open(target.clone())
            .unwrap()
            .add_approvals(&state.entries[0].package.name, approvals)
            .unwrap();
        let path = target.path.clone();
        state.global_target = Some(target);
        path
    }

    #[test]
    fn blanket_approvals_cover_every_queued_version_without_verification() {
        let (root, state) = fixture();
        let policy = "# Manual approval\n[approved]\nduplicate = 'approved'\n";
        fs::write(&state.approval_target.path, policy).unwrap();
        let mut state = reopen(&state);
        let indices: Vec<_> = state
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| (entry.package.name == "duplicate").then_some(index))
            .collect();
        assert_eq!(indices.len(), 2);
        assert_eq!(state.summary().already_approved, 2);
        for index in indices {
            state.open(index);
            let entry = &state.entries[index];
            assert!(entry.already_approved());
            assert!(!entry.actionable());
            assert!(!entry.needs_verification());
            assert!(!entry.checking);
            assert!(entry.verification.is_none());
            assert_eq!(entry.status(), "already approved");
            assert!(state.loader.is_none());
            let app = state.app.as_ref().unwrap();
            assert!(matches!(app.baseline, Baseline::Blanket));
            assert!(!app.can_decide());
            assert!(app.verification_message.is_none());
            for decision in [
                Decision::ApproveFingerprint,
                Decision::PinVersion,
                Decision::Reject,
            ] {
                assert!(state.decide(decision).is_err());
            }
            assert_eq!(
                fs::read_to_string(&state.approval_target.path).unwrap(),
                policy
            );
        }
        assert!(!root.path().join("target").exists());
    }

    #[test]
    fn manual_blanket_refresh_clears_decisions_and_removal_restores_review() {
        let (_root, mut state) = fixture();
        let name = state.entries[0].package.name.clone();
        state.decide(Decision::Reject).unwrap();
        state.open(0);
        assert_eq!(state.entries[0].decision, Some(Decision::Reject));
        let policy = format!("[approved]\n{name} = 'approved'\n");
        fs::write(&state.approval_target.path, &policy).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        state.poll();
        assert_eq!(state.entries[0].decision, None);
        assert_eq!(state.entries[0].status(), "already approved");
        assert!(matches!(
            state.app.as_ref().unwrap().baseline,
            Baseline::Blanket
        ));
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert!(state.loader.is_none());
        assert_eq!(
            fs::read_to_string(&state.approval_target.path).unwrap(),
            policy
        );

        fs::write(&state.approval_target.path, "[approved]\n").unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        state.poll();
        assert!(state.entries[0].actionable());
        assert!(state.app.as_ref().unwrap().can_decide());
        assert!(matches!(
            state.app.as_ref().unwrap().baseline,
            Baseline::NewPackage
        ));
    }

    #[test]
    fn global_blanket_is_informational_until_selected_as_the_write_target() {
        let (_root, mut state) = fixture();
        let name = &state.entries[0].package.name;
        let target = ApprovalTarget {
            path: state.workspace_root.join("global-approvals.toml"),
            scope: crate::config::ApprovalScope::Global,
        };
        let policy = format!("[approved]\n{name} = 'approved'\n");
        fs::write(&target.path, &policy).unwrap();
        state.global_target = Some(target.clone());
        let mut state = reopen(&state);
        assert!(state.entries[0].globally_approved());
        assert!(!state.entries[0].needs_global_verification());
        assert!(!state.entries[0].global_checking);
        assert!(state.entries[0].global_verification.is_none());
        assert!(state.entries[0].actionable());
        assert_eq!(state.entries[0].status(), "approved globally");
        assert_eq!(
            state.app.as_ref().unwrap().displayed_approval_scope(),
            Some(crate::config::ApprovalScope::Global)
        );
        assert!(state.app.as_ref().unwrap().can_decide());
        state.decide(Decision::PinVersion).unwrap();
        assert_eq!(fs::read_to_string(&target.path).unwrap(), policy);
        let workspace_policy = fs::read(&state.approval_target.path).unwrap();

        let workspace_path = state.approval_target.path.clone();
        state.approval_target = target.clone();
        state.global_target = None;
        let mut state = reopen(&state);
        state.open(0);
        assert!(state.entries[0].already_approved());
        assert!(!state.entries[0].needs_verification());
        assert!(matches!(
            state.app.as_ref().unwrap().baseline,
            Baseline::Blanket
        ));
        assert_eq!(
            state.app.as_ref().unwrap().displayed_approval_scope(),
            Some(crate::config::ApprovalScope::Global)
        );
        for decision in [
            Decision::ApproveFingerprint,
            Decision::PinVersion,
            Decision::Reject,
        ] {
            assert!(state.decide(decision).is_err());
        }
        assert_eq!(fs::read_to_string(&target.path).unwrap(), policy);
        assert_eq!(fs::read(&workspace_path).unwrap(), workspace_policy);
    }

    fn global_fingerprint_fixture() -> (tempfile::TempDir, QueueState, PathBuf) {
        let (root, mut state, artifact) = fingerprint_fixture();
        let approvals =
            super::super::fingerprint::approvals(&state.app.as_ref().unwrap().session).unwrap();
        with_global_approvals(&mut state, &approvals);
        let reopened = reopen(&state);
        (root, reopened, artifact)
    }

    #[test]
    fn global_version_is_informational_and_local_saves_remain_isolated() {
        let (root, mut state) = fixture();
        let version = state.entries[0].package.version.clone();
        let global = with_global_approvals(&mut state, &[ApprovedVersion::SemVer(version.clone())]);
        let global_contents = fs::read_to_string(&global).unwrap();
        let mut state = reopen(&state);
        assert!(state.entries[0].globally_approved());
        assert!(state.entries[0].actionable());
        assert!(!state.entries[0].already_approved());
        assert!(state.app.as_ref().unwrap().globally_approved);
        assert!(state.app.as_ref().unwrap().can_decide());
        assert_eq!(state.app.as_ref().unwrap().session.package.approval, None);
        assert_eq!(state.summary().already_approved, 0);
        assert!(!root.path().join("approved-builds.toml").exists());
        state.decide(Decision::PinVersion).unwrap();
        state.open(0);
        assert!(state.entries[0].globally_approved());
        assert!(state.entries[0].already_approved());
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert_eq!(state.entries[0].status(), "approved");
        assert_eq!(fs::read_to_string(&global).unwrap(), global_contents);
        let local = ConfigFile::open(state.approval_target.clone()).unwrap();
        assert!(local.config().approved[&state.entries[0].package.name].allows_version(&version));
        let reopened = reopen(&state);
        assert_eq!(reopened.entries[0].status(), "already approved");
    }

    #[test]
    fn removed_or_invalid_global_documents_clear_status_without_blocking_local_review() {
        let (_root, mut state) = fixture();
        let version = state.entries[0].package.version.clone();
        let global = with_global_approvals(&mut state, &[ApprovedVersion::SemVer(version)]);
        let contents = fs::read_to_string(&global).unwrap();
        let mut state = reopen(&state);
        assert!(state.entries[0].globally_approved());
        fs::remove_file(&global).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        state.poll();
        assert!(!state.entries[0].globally_approved());
        assert!(!state.app.as_ref().unwrap().globally_approved);
        fs::write(&global, &contents).unwrap();
        state.open(0);
        assert!(state.app.as_ref().unwrap().globally_approved);
        fs::write(&global, "not valid [toml").unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        state.poll();
        assert!(!state.app.as_ref().unwrap().globally_approved);
        assert!(state.entries[0].actionable());
        state.decide(Decision::PinVersion).unwrap();
        assert!(state.entries[0].already_approved());
        assert_eq!(fs::read_to_string(&global).unwrap(), "not valid [toml");
    }

    #[test]
    fn global_fingerprints_require_verification_and_detect_changed_helpers() {
        let (_root, mut state, artifact) = global_fingerprint_fixture();
        assert!(!state.entries[0].globally_approved());
        wait_for(&mut state, |state| state.entries[0].globally_approved());
        assert!(state.app.as_ref().unwrap().globally_approved);
        assert!(state.entries[0].actionable());
        assert!(state.app.as_ref().unwrap().can_decide());
        assert!(
            !state
                .app
                .as_ref()
                .unwrap()
                .is_current_fingerprint_approved()
        );
        assert_eq!(state.summary().already_approved, 0);
        let script = &state.entries[0].package.scripts[0];
        fs::write(
            script.path.with_file_name("helper.rs"),
            "const INPUT: u8 = 2;\n",
        )
        .unwrap();
        let dep_info = fs::read_to_string(&artifact).unwrap();
        fs::write(&artifact, dep_info).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        wait_for(&mut state, |state| {
            matches!(
                state.entries[0].global_verification,
                Some(Verification::Mismatch)
            )
        });
        assert!(!state.app.as_ref().unwrap().globally_approved);
        assert!(state.entries[0].actionable());
    }

    #[test]
    fn global_fingerprint_status_recovers_after_artifacts_reappear() {
        let (_root, mut state, artifact) = global_fingerprint_fixture();
        wait_for(&mut state, |state| state.entries[0].globally_approved());
        let dep_info = fs::read_to_string(&artifact).unwrap();
        fs::remove_file(&artifact).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        wait_for(&mut state, |state| {
            matches!(
                state.entries[0].global_verification,
                Some(Verification::Unavailable(_))
            )
        });
        assert!(!state.app.as_ref().unwrap().globally_approved);
        assert_eq!(state.summary().unresolved, 0);
        fs::write(&artifact, dep_info).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        wait_for(&mut state, |state| state.entries[0].globally_approved());
        assert!(state.app.as_ref().unwrap().globally_approved);
    }

    #[test]
    fn late_global_results_cannot_reuse_removed_approvals_or_changed_sources() {
        let (_root, mut state, _artifact) = global_fingerprint_fixture();
        let result = completed_result(&state);
        assert!(matches!(result.verification, Verification::Verified(_)));
        let path = state.global_target.as_ref().unwrap().path.clone();
        let contents = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();
        state.open(0);
        assert!(!state.apply_result(result));
        assert!(!state.app.as_ref().unwrap().globally_approved);
        fs::write(&path, contents).unwrap();
        state.open(0);
        let result = completed_result(&state);
        fs::write(
            &state.entries[0].package.scripts[0].path,
            "fn changed() {}\n",
        )
        .unwrap();
        state.apply_result(result);
        assert!(!state.entries[0].globally_approved());
        assert!(!state.app.as_ref().unwrap().globally_approved);
        assert!(state.app.as_ref().unwrap().can_decide());
    }

    #[test]
    fn selected_and_global_fingerprints_use_independent_verification_jobs() {
        let (_root, mut state, _artifact) = saved_fingerprint_fixture();
        let approval = state.entries[0].approvals.clone().unwrap();
        with_global_approvals(&mut state, &[approval]);
        let mut state = reopen(&state);
        wait_for(&mut state, |state| {
            state.entries[0].already_approved() && state.entries[0].globally_approved()
        });
        assert_eq!(state.entries[0].status(), "already approved");
        assert!(state.app.as_ref().unwrap().globally_approved);
        assert!(!state.app.as_ref().unwrap().can_decide());
    }

    #[test]
    fn global_navigation_reopens_additively_and_save_conflicts_stay_undecided() {
        let (root, metadata) = super::super::tests::fixture();
        let external = tempfile::tempdir().unwrap();
        let global = external.path().join("approvals.toml");
        let old = format!("{}+{}#1.0.0", "0".repeat(64), "0".repeat(16));
        fs::write(&global, format!("[approved]\ncustom-script='{old}'\n")).unwrap();
        let options = crate::args::ReviewOptions {
            global: true,
            ..Default::default()
        };
        let queue = ReviewQueue::from_metadata(&metadata, &options, Some(&global)).unwrap();
        let notes = queue.policy_notes.clone();
        let mut state = QueueState::new(queue);
        let index = state
            .entries
            .iter()
            .position(|entry| entry.package.name == "custom-script")
            .unwrap();
        state.open(index);
        let script = state.entries[index].package.scripts[0].clone();
        let artifact = root.path().join(format!(
            "target/debug/build/custom-script/out/{}.d",
            script.name.replace('-', "_")
        ));
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, format!("binary: {}\n", script.path.display())).unwrap();
        state.navigate(false);
        state.navigate(true);
        assert_eq!(state.selected, index);
        assert_eq!(
            state.app.as_ref().unwrap().session.workspace_root,
            metadata.workspace_root.as_std_path()
        );
        assert_eq!(state.app.as_ref().unwrap().session.policy_notes, notes);
        assert_eq!(
            state.app.as_ref().unwrap().session.config.target().scope,
            crate::config::ApprovalScope::Global
        );
        state.decide(Decision::ApproveFingerprint).unwrap();
        assert_eq!(
            state.entries[index].decision,
            Some(Decision::ApproveFingerprint)
        );
        let saved = fs::read_to_string(&global).unwrap();
        assert!(saved.contains(&old));
        let config = ConfigFile::open(state.approval_target.clone()).unwrap();
        assert!(
            matches!(&config.config().approved["custom-script"], ApprovedVersion::Multiple(values) if values.len() == 2)
        );
        assert!(!root.path().join("approved-builds.toml").exists());
        let mut reopened = reopen(&state);
        assert_eq!(
            reopened.approval_target.scope,
            crate::config::ApprovalScope::Global
        );
        assert_eq!(
            reopened.approval_target.path,
            global.canonicalize().unwrap()
        );
        assert_eq!(reopened.workspace_root, state.workspace_root);
        let next = reopened
            .entries
            .iter()
            .position(|entry| entry.package.name == "default-script")
            .unwrap();
        reopened.open(next);
        fs::write(&global, format!("{saved}\n# concurrent edit\n")).unwrap();
        assert!(reopened.decide(Decision::PinVersion).is_err());
        assert_eq!(reopened.selected, next);
        assert_eq!(reopened.entries[next].decision, None);
        assert_eq!(reopened.app.as_ref().unwrap().decision, None);
        assert_eq!(
            fs::read_to_string(&global).unwrap(),
            format!("{saved}\n# concurrent edit\n")
        );
    }

    fn saved_fingerprint_fixture() -> (tempfile::TempDir, QueueState, PathBuf) {
        let (root, mut state, artifact) = fingerprint_fixture();
        state.decide(Decision::ApproveFingerprint).unwrap();
        let reopened = reopen(&state);
        (root, reopened, artifact)
    }

    fn completed_result(state: &QueueState) -> verification::Result {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = state.verifier.as_ref().unwrap().poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "verification timed out");
            std::thread::park_timeout(Duration::from_millis(5));
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
        state.app.as_mut().unwrap().set_viewport(1, 10);
        state.app.as_mut().unwrap().scroll(1, 0);
        let top = state.app.as_ref().unwrap().top;
        state.navigate(false);
        state.navigate(true);
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
        state.navigate(false);
        state.navigate(true);
        assert!(matches!(
            state.app.as_ref().unwrap().baseline,
            Baseline::Ready(_)
        ));
        assert!(state.loader.is_none());
    }

    #[test]
    fn saved_approvals_verify_offscreen_and_remain_stable_during_navigation() {
        let (_root, mut state, _artifact) = saved_fingerprint_fixture();
        state.navigate(false);
        wait_for(&mut state, |state| state.entries[0].already_approved());
        assert_eq!(state.selected, 1);
        assert!(!state.entries[1].already_approved());
        assert!(state.app.as_ref().unwrap().can_decide());
        for _ in 0..20 {
            state.navigate(true);
            assert!(state.entries[0].already_approved());
            assert!(!state.app.as_ref().unwrap().can_decide());
            assert!(!state.app.as_ref().unwrap().verification_pending);
            state.navigate(false);
            assert!(state.entries[0].already_approved());
            state.poll();
        }
        assert_eq!(state.summary().already_approved, 1);
        assert_eq!(state.summary().approved, 0);
    }

    #[test]
    fn startup_verifies_packages_that_are_never_selected() {
        let (_root, mut saved, _artifact) = fingerprint_fixture();
        saved.decide(Decision::ApproveFingerprint).unwrap();
        let mut packages: Vec<_> = saved
            .entries
            .iter()
            .map(|entry| entry.package.clone())
            .collect();
        packages.swap(0, 1);
        let mut state = QueueState::new(ReviewQueue {
            packages,
            config: ConfigFile::open(saved.approval_target.clone()).unwrap(),
            global_target: saved.global_target.clone(),
            build_directory: saved.workspace_root.join("target"),
            workspace_root: saved.workspace_root.clone(),
            policy_notes: saved.policy_notes.clone(),
        });
        wait_for(&mut state, |state| state.entries[1].already_approved());
        assert_eq!(state.selected, 0);
        assert!(!state.entries[0].already_approved());
        assert_eq!(state.summary().already_approved, 1);
    }

    #[test]
    fn current_verification_prevents_unneeded_history_loading() {
        let (_root, mut state, _artifact) = saved_fingerprint_fixture();
        wait_for(&mut state, |state| state.entries[0].already_approved());
        assert!(matches!(
            state.app.as_ref().unwrap().baseline,
            Baseline::CurrentFingerprint(_)
        ));
        assert!(state.loader.is_none());
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert_eq!(state.app.as_ref().unwrap().decision, None);
    }

    #[test]
    fn background_rechecks_invalidate_changed_helpers_without_navigation() {
        let (_root, mut state, artifact) = saved_fingerprint_fixture();
        wait_for(&mut state, |state| state.entries[0].already_approved());
        let script = state.entries[0].package.scripts[0].path.clone();
        let displayed = state.app.as_ref().unwrap().sources();
        fs::write(script.with_file_name("helper.rs"), "const INPUT: u8 = 2;\n").unwrap();
        let dep_info = fs::read_to_string(&artifact).unwrap();
        fs::write(&artifact, dep_info).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        wait_for(&mut state, |state| {
            matches!(state.entries[0].verification, Some(Verification::Mismatch))
        });
        assert_eq!(state.app.as_ref().unwrap().sources(), displayed);
        assert!(!state.entries[0].already_approved());
        assert!(state.entries[0].actionable());
        assert!(state.app.as_ref().unwrap().can_decide());
        assert_eq!(state.summary().already_approved, 0);
    }

    #[test]
    fn changed_displayed_sources_cannot_reuse_a_cached_approval() {
        let (_root, mut state, _artifact) = saved_fingerprint_fixture();
        wait_for(&mut state, |state| state.entries[0].already_approved());
        state.navigate(false);
        fs::write(
            &state.entries[0].package.scripts[0].path,
            "fn changed() {}\n",
        )
        .unwrap();
        state.navigate(true);
        assert!(!state.entries[0].already_approved());
        assert!(state.app.as_ref().unwrap().can_decide());
        assert_eq!(
            state.app.as_ref().unwrap().sources()[0].contents,
            "fn changed() {}\n"
        );
    }

    #[test]
    fn results_from_replaced_approval_snapshots_are_discarded() {
        let (_root, mut state, _artifact) = saved_fingerprint_fixture();
        let result = completed_result(&state);
        assert!(matches!(result.verification, Verification::Verified(_)));
        let mut config = ConfigFile::open(state.approval_target.clone()).unwrap();
        config
            .set_approval(
                &state.entries[0].package.name,
                ApprovedVersion::Fingerprint(format!("{:064x}+{:016x}#2.0.0", 1, 2)),
            )
            .unwrap();
        state.open(0);
        assert!(!state.apply_result(result));
        assert!(!state.entries[0].already_approved());
        wait_for(&mut state, |state| {
            matches!(state.entries[0].verification, Some(Verification::Mismatch))
        });
        assert!(state.app.as_ref().unwrap().can_decide());
    }

    #[test]
    fn late_verification_cannot_overwrite_a_newer_rejection() {
        let (_root, mut state, _artifact) = saved_fingerprint_fixture();
        let result = completed_result(&state);
        state.decide(Decision::Reject).unwrap();
        assert!(!state.apply_result(result));
        assert_eq!(state.entries[0].decision, Some(Decision::Reject));
        assert!(!state.entries[0].already_approved());
        assert_eq!(state.summary().rejected, 1);
    }

    #[test]
    fn late_verification_cannot_authorize_newly_displayed_sources() {
        let (_root, mut state, _artifact) = saved_fingerprint_fixture();
        let result = completed_result(&state);
        fs::write(
            &state.entries[0].package.scripts[0].path,
            "fn changed() {}\n",
        )
        .unwrap();
        state.apply_result(result);
        assert!(!state.entries[0].already_approved());
        assert!(state.app.as_ref().unwrap().can_decide());
        assert!(state.app.as_ref().unwrap().verification_pending);
    }

    #[test]
    fn missing_artifacts_are_unresolved_and_recover_after_rebuilding() {
        let (_root, mut state, artifact) = saved_fingerprint_fixture();
        wait_for(&mut state, |state| state.entries[0].already_approved());
        let dep_info = fs::read_to_string(&artifact).unwrap();
        fs::remove_file(&artifact).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        wait_for(&mut state, |state| {
            matches!(
                state.entries[0].verification,
                Some(Verification::Unavailable(_))
            )
        });
        assert_eq!(state.summary().unresolved, 1);
        assert!(state.app.as_ref().unwrap().can_decide());
        fs::write(&artifact, dep_info).unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        wait_for(&mut state, |state| state.entries[0].already_approved());
        assert!(!state.app.as_ref().unwrap().can_decide());
        assert_eq!(state.summary().unresolved, 0);
    }

    #[test]
    fn cached_view_keeps_save_conflict_detection() {
        let (root, mut state) = fixture();
        state.navigate(false);
        state.navigate(true);
        let external = "[approved]\nother='1.0.0'\n";
        fs::write(root.path().join("approved-builds.toml"), external).unwrap();
        assert!(state.decide(Decision::PinVersion).is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("approved-builds.toml")).unwrap(),
            external
        );
    }

    #[test]
    fn offscreen_approval_changes_invalidate_the_complete_snapshot() {
        let (_root, mut saved, _artifact) = fingerprint_fixture();
        saved.decide(Decision::ApproveFingerprint).unwrap();
        let Some(ApprovedVersion::Fingerprint(fingerprint)) = &saved.entries[0].approvals else {
            panic!("expected a saved fingerprint");
        };
        let hashes = fingerprint.split_once('#').unwrap().0;
        fs::write(
            &saved.approval_target.path,
            format!("[approved]\ncustom-script=['{hashes}#9.8.7','1.0.0']\n"),
        )
        .unwrap();
        let mut state = reopen(&saved);
        let representative = state.entries[0].package.approval.clone();
        wait_for(&mut state, |state| state.entries[0].already_approved());
        state.navigate(false);
        fs::write(
            &state.approval_target.path,
            "[approved]\ncustom-script='1.0.0'\n",
        )
        .unwrap();
        state.last_refresh -= RECHECK_INTERVAL;
        assert!(state.poll());
        assert_eq!(state.selected, 1);
        assert_eq!(state.entries[0].package.approval, representative);
        assert!(!state.entries[0].already_approved());
        assert_eq!(state.summary().already_approved, 0);
        state.navigate(true);
        assert!(
            !state
                .app
                .as_ref()
                .unwrap()
                .is_current_fingerprint_approved()
        );
        assert!(state.app.as_ref().unwrap().can_decide());
    }

    #[test]
    fn view_cache_is_bounded() {
        let (_root, metadata) = super::super::tests::fixture();
        let mut queue =
            ReviewQueue::from_metadata(&metadata, &crate::args::ReviewOptions::default(), None)
                .unwrap();
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
        let queue =
            ReviewQueue::from_metadata(&metadata, &crate::args::ReviewOptions::default(), None)
                .unwrap();
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
        state.decide(Decision::PinVersion).unwrap();
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
        let reopened = QueueState::new(
            ReviewQueue::from_metadata(&metadata, &crate::args::ReviewOptions::default(), None)
                .unwrap(),
        );
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
        let mut state = QueueState::new(
            ReviewQueue::from_metadata(&metadata, &crate::args::ReviewOptions::default(), None)
                .unwrap(),
        );
        assert_eq!(state.selected, 1);
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
        assert!(!state.decide(Decision::PinVersion).unwrap());
        assert_eq!(state.entries[state.selected].package.name, "default-script");
        assert!(!state.decide(Decision::PinVersion).unwrap());
        assert_eq!(state.entries[state.selected].package.name, "duplicate");
        assert!(!state.decide(Decision::PinVersion).unwrap());
        assert!(state.decide(Decision::PinVersion).unwrap());
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
        state.decide(Decision::PinVersion).unwrap();
        state.navigate(true);
        assert!(!state.app.as_ref().unwrap().can_decide());
        state.navigate(true);
        assert_eq!(state.entries[state.selected].package.name, "duplicate");
        assert!(state.app.as_ref().unwrap().can_decide());
        assert!(state.decide(Decision::PinVersion).is_ok());
    }

    #[test]
    fn conflicts_on_save_do_not_advance_or_change_decisions() {
        let (root, mut state) = fixture();
        let external = "[approved]\nother='1.0.0'\n";
        fs::write(root.path().join("approved-builds.toml"), external).unwrap();
        let index = state.selected;
        assert!(state.decide(Decision::PinVersion).is_err());
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
