use std::path::PathBuf;

use cargo_metadata::{Metadata, Package, PackageId, Source};
use eyre::Context;

use crate::{
    args::ReviewOptions,
    config::{ApprovedVersion, edit::ConfigFile},
};

mod app;
mod diff;
mod discovery;
mod fingerprint;
mod highlight;
mod input;
mod queue;
mod sources;
mod tui;
mod ui;

pub struct ReviewSession {
    pub package: ReviewPackage,
    pub config: ConfigFile,
}

#[derive(Clone, Debug)]
pub struct ReviewPackage {
    pub id: PackageId,
    pub name: String,
    pub version: semver::Version,
    pub source: Option<Source>,
    pub manifest_path: PathBuf,
    pub approval: Option<ApprovedVersion>,
    pub scripts: Vec<BuildScript>,
}

#[derive(Clone, Debug)]
pub struct BuildScript {
    pub name: String,
    pub path: PathBuf,
}

pub fn run(package: Option<&str>, options: &ReviewOptions) -> eyre::Result<()> {
    // Full metadata includes dependency packages, not just workspace members.
    // This resolves/downloads sources but does not compile or execute scripts.
    let metadata = discovery::metadata_command(options)
        .exec()
        .context("could not load workspace metadata for review")?;
    if let Some(selector) = package {
        let (name, version) = package_selector(selector)?;
        let candidates = discovery::resolved_packages(&metadata)?;
        let named: Vec<_> = candidates
            .into_iter()
            .filter(|p| p.name.as_str() == name && version.as_ref().is_none_or(|v| v == &p.version))
            .collect();
        if !named.is_empty()
            && named
                .iter()
                .all(|p| crate::workspace::is_member(&metadata, p))
        {
            println!(
                "Package `{name}` belongs to the current workspace; its build scripts do not require approval."
            );
            return Ok(());
        }
        tui::run(ReviewSession::from_metadata(&metadata, selector)?)
    } else {
        let queue = discovery::ReviewQueue::from_metadata(&metadata)?;
        tui::run_queue(queue)
    }
}

impl ReviewSession {
    fn from_metadata(metadata: &Metadata, name: &str) -> eyre::Result<Self> {
        let (name, version) = package_selector(name)?;
        let mut candidates: Vec<_> = discovery::resolved_packages(metadata)?
            .into_iter()
            .filter(|package| package.name.as_str() == name)
            .filter(|package| version.as_ref().is_none_or(|v| v == &package.version))
            .filter(|package| !crate::workspace::is_member(metadata, package))
            .collect();
        candidates.sort_by(|a, b| {
            a.version
                .cmp(&b.version)
                .then_with(|| a.id.repr.cmp(&b.id.repr))
        });
        eyre::ensure!(
            !candidates.is_empty(),
            "package `{name}` was not found in the workspace dependency graph"
        );
        eyre::ensure!(
            candidates.len() == 1,
            "package name `{name}` is ambiguous; use NAME@VERSION to select a version (multiple sources may still be ambiguous). Matching packages:\n{}",
            candidates
                .iter()
                .map(|package| format!("  {} v{} ({})", package.name, package.version, package.id))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let package = candidates[0];
        // Review discovers the consuming workspace itself, rather than relying
        // on build-runner environment variables or a dependency's manifest dir.
        let config = ConfigFile::for_workspace(metadata.workspace_root.as_std_path())?;
        let package = ReviewPackage::from_package(package, &config);
        eyre::ensure!(
            !package.scripts.is_empty(),
            "package `{name}` v{} has no build scripts",
            package.version
        );
        Ok(Self { package, config })
    }
}

impl ReviewPackage {
    fn from_package(package: &Package, config: &ConfigFile) -> Self {
        let mut scripts: Vec<_> = package
            .targets
            .iter()
            .filter(|target| target.is_custom_build())
            .map(|target| BuildScript {
                name: target.name.clone(),
                path: target.src_path.clone().into_std_path_buf(),
            })
            .collect();
        scripts.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.name.cmp(&b.name)));
        let approval = config
            .config()
            .approved
            .get(package.name.as_str())
            .and_then(|a| a.for_review(&package.version));
        Self {
            id: package.id.clone(),
            name: package.name.to_string(),
            version: package.version.clone(),
            source: package.source.clone(),
            manifest_path: package.manifest_path.clone().into_std_path_buf(),
            approval,
            scripts,
        }
    }
}

fn package_selector(selector: &str) -> eyre::Result<(&str, Option<semver::Version>)> {
    match selector.rsplit_once('@') {
        Some((name, version)) => Ok((
            name,
            Some(
                version
                    .parse()
                    .context("invalid package version in selector")?,
            ),
        )),
        None => Ok((selector, None)),
    }
}

#[cfg(test)]
mod tests {
    use cargo_metadata::MetadataCommand;
    use std::{fs, path::Path};

    use super::*;

    #[test]
    fn version_selector_disambiguates_packages() {
        let (_root, metadata) = fixture();
        let session = ReviewSession::from_metadata(&metadata, "duplicate@2.0.0").unwrap();
        assert_eq!(
            session.package.version,
            "2.0.0".parse::<semver::Version>().unwrap()
        );
        assert!(ReviewSession::from_metadata(&metadata, "duplicate@3.0.0").is_err());
        assert!(ReviewSession::from_metadata(&metadata, "duplicate@invalid").is_err());
    }

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub(super) fn fixture() -> (tempfile::TempDir, Metadata) {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            r#"
            [package]
            name = "app"
            version = "0.1.0"
            edition = "2024"
            [workspace]
            exclude = ["deps/default", "deps/custom", "deps/none", "deps/v1", "deps/v2"]
            [dependencies]
            default-script = { path = "deps/default" }
            custom-script = { path = "deps/custom" }
            no-script = { path = "deps/none" }
            duplicate-one = { package = "duplicate", path = "deps/v1" }
            duplicate-two = { package = "duplicate", path = "deps/v2" }
        "#,
        );
        write(root.path(), "src/lib.rs", "");
        for (directory, name, version, build) in [
            ("default", "default-script", "1.0.0", ""),
            (
                "custom",
                "custom-script",
                "2.0.0",
                "build = 'scripts/custom.rs'",
            ),
            ("none", "no-script", "1.0.0", "build = false"),
            ("v1", "duplicate", "1.0.0", ""),
            ("v2", "duplicate", "2.0.0", ""),
        ] {
            write(
                root.path(),
                &format!("deps/{directory}/Cargo.toml"),
                &format!(
                    "[package]\nname = '{name}'\nversion = '{version}'\nedition = '2024'\n{build}\n[workspace]\n"
                ),
            );
            write(root.path(), &format!("deps/{directory}/src/lib.rs"), "");
            let script = if directory == "custom" {
                "scripts/custom.rs"
            } else {
                "build.rs"
            };
            // Metadata must never compile or execute these files.
            write(
                root.path(),
                &format!("deps/{directory}/{script}"),
                "compile_error!(\"must not compile during review\");",
            );
        }
        let metadata = MetadataCommand::new()
            .manifest_path(root.path().join("Cargo.toml"))
            .other_options(vec!["--offline".into()])
            .exec()
            .unwrap();
        (root, metadata)
    }

    #[test]
    fn single_package_review_workflow_persists_decisions_and_reopens() {
        use super::{
            app::{App, Decision},
            diff::FileStatus,
            input::{self, Action},
            sources::{Baseline, PackageSources, ScriptSource},
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (root, metadata) = fixture();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let mut app =
            App::new(ReviewSession::from_metadata(&metadata, "default-script").unwrap()).unwrap();
        assert_eq!(app.file().status, FileStatus::Added);
        assert_eq!(
            input::handle_key(&mut app, key('a')),
            Action::Decide(Decision::Approve)
        );
        app.decide(Decision::Approve).unwrap();
        let saved = fs::read(app.session.config.path()).unwrap();
        let mut reopened =
            App::new(ReviewSession::from_metadata(&metadata, "default-script").unwrap()).unwrap();
        assert!(reopened.is_current_version_approved());
        assert_eq!(input::handle_key(&mut reopened, key('r')), Action::Continue);
        assert_eq!(input::handle_key(&mut reopened, key('q')), Action::Quit);
        assert_eq!(fs::read(reopened.session.config.path()).unwrap(), saved);

        let manifest_path = root.path().join("deps/default/Cargo.toml");
        let manifest = fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("version = '1.0.0'", "version = '1.1.0'");
        fs::write(&manifest_path, manifest).unwrap();
        let upgraded = MetadataCommand::new()
            .manifest_path(root.path().join("Cargo.toml"))
            .other_options(vec!["--offline".into()])
            .exec()
            .unwrap();
        let mut app =
            App::new(ReviewSession::from_metadata(&upgraded, "default-script").unwrap()).unwrap();
        // Supply the fixture's previous source snapshot; real registry retrieval
        // is exercised separately by the source-loading tests.
        app.set_baseline(Baseline::Ready(PackageSources {
            version: "1.0.0".parse().unwrap(),
            scripts: vec![ScriptSource {
                path: "build.rs".into(),
                contents: fs::read_to_string(root.path().join("deps/default/build.rs")).unwrap(),
            }],
        }))
        .unwrap();
        assert_eq!(app.file().status, FileStatus::Unchanged);
        assert!(app.can_decide());
        assert_eq!(
            input::handle_key(&mut app, key('r')),
            Action::Decide(Decision::Reject)
        );
        app.decide(Decision::Reject).unwrap();
        assert_eq!(fs::read(app.session.config.path()).unwrap(), saved);

        let mut app =
            App::new(ReviewSession::from_metadata(&upgraded, "default-script").unwrap()).unwrap();
        app.decide(Decision::Approve).unwrap();
        let reopened = ReviewSession::from_metadata(&upgraded, "default-script").unwrap();
        assert!(
            matches!(reopened.package.approval, Some(ApprovedVersion::SemVer(v)) if v == semver::Version::new(1, 1, 0))
        );
        assert!(!root.path().join("target").exists());
    }

    #[test]
    fn discovers_dependency_scripts_and_workspace_approvals() {
        let (root, metadata) = fixture();
        let session = ReviewSession::from_metadata(&metadata, "default-script").unwrap();
        assert_eq!(session.package.scripts.len(), 1);
        assert_eq!(
            session.package.scripts[0].path,
            root.path().join("deps/default/build.rs")
        );
        assert!(session.package.approval.is_none());
        assert!(!root.path().join("approved-builds.toml").exists());

        write(
            root.path(),
            "approved-builds.toml",
            "[approved]\ncustom-script = '1.0.0'\n",
        );
        let session = ReviewSession::from_metadata(&metadata, "custom-script").unwrap();
        assert_eq!(session.package.version, semver::Version::new(2, 0, 0));
        assert_eq!(
            session.package.scripts[0].path,
            root.path().join("deps/custom/scripts/custom.rs")
        );
        assert!(
            matches!(session.package.approval, Some(ApprovedVersion::SemVer(v)) if v == semver::Version::new(1, 0, 0))
        );
        assert_eq!(
            session.config.path(),
            root.path().join("approved-builds.toml")
        );
        assert!(!root.path().join("target").exists());
    }

    #[test]
    fn reports_missing_scriptless_and_ambiguous_packages() {
        let (_root, metadata) = fixture();
        for (name, message) in [
            ("missing", "not found"),
            ("no-script", "has no build scripts"),
            ("duplicate", "ambiguous"),
        ] {
            let error = ReviewSession::from_metadata(&metadata, name)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(message), "{error}");
            if name == "duplicate" {
                assert!(error.contains("duplicate v1.0.0"));
                assert!(error.contains("duplicate v2.0.0"));
                assert!(error.contains("deps/v1"));
                assert!(error.contains("deps/v2"));
            }
        }
    }

    #[test]
    fn retains_every_custom_build_target() {
        let (_root, mut metadata) = fixture();
        let package = metadata
            .packages
            .iter_mut()
            .find(|p| p.name.as_str() == "custom-script")
            .unwrap();
        let mut second = package
            .targets
            .iter()
            .find(|t| t.is_custom_build())
            .unwrap()
            .clone();
        second.name = "build-script-second".into();
        second.src_path = second.src_path.parent().unwrap().join("second.rs");
        package.targets.push(second);
        let session = ReviewSession::from_metadata(&metadata, "custom-script").unwrap();
        assert_eq!(session.package.scripts.len(), 2);
        assert!(session.package.scripts[0].path.ends_with("custom.rs"));
        assert!(session.package.scripts[1].path.ends_with("second.rs"));
    }
}
