use std::path::PathBuf;

use cargo_metadata::{Metadata, MetadataCommand, PackageId, Source};
use eyre::{Context, ContextCompat};

use crate::config::{ApprovedVersion, edit::ConfigFile};

mod app;
mod highlight;
mod input;
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

pub fn run(package: Option<&str>) -> eyre::Result<()> {
    let name = package.context(
        "reviewing all packages is not implemented yet; use `cargo guard review <PACKAGE>`",
    )?;
    // Full metadata includes dependency packages, not just workspace members.
    // This resolves/downloads sources but does not compile or execute scripts.
    let metadata = MetadataCommand::new()
        .exec()
        .context("could not load workspace metadata for review")?;
    let session = ReviewSession::from_metadata(&metadata, name)?;
    tui::run(session)
}

impl ReviewSession {
    fn from_metadata(metadata: &Metadata, name: &str) -> eyre::Result<Self> {
        let mut candidates: Vec<_> = metadata
            .packages
            .iter()
            .filter(|package| package.name.as_str() == name)
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
            "package name `{name}` is ambiguous; matching packages:\n{}",
            candidates
                .iter()
                .map(|package| format!("  {} v{} ({})", package.name, package.version, package.id))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let package = candidates[0];
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
        eyre::ensure!(
            !scripts.is_empty(),
            "package `{name}` v{} has no build scripts",
            package.version
        );

        // Review discovers the consuming workspace itself, rather than relying
        // on build-runner environment variables or a dependency's manifest dir.
        let config = ConfigFile::for_workspace(metadata.workspace_root.as_std_path())?;
        let approval = config.config().approved.get(name).cloned();
        Ok(Self {
            package: ReviewPackage {
                id: package.id.clone(),
                name: package.name.to_string(),
                version: package.version.clone(),
                source: package.source.clone(),
                manifest_path: package.manifest_path.clone().into_std_path_buf(),
                approval,
                scripts,
            },
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, Metadata) {
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
