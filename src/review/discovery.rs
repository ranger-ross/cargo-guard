use std::collections::{BTreeMap, BTreeSet};

use cargo_metadata::{CargoOpt, Metadata, MetadataCommand, Package};
use eyre::ContextCompat;

use super::ReviewPackage;
use crate::{args::ReviewOptions, config::edit::ConfigFile};

pub fn metadata_command(options: &ReviewOptions) -> MetadataCommand {
    let mut command = MetadataCommand::new();
    if options.all_features {
        command.features(CargoOpt::AllFeatures);
    }
    if options.no_default_features {
        command.features(CargoOpt::NoDefaultFeatures);
    }
    let features: Vec<_> = options
        .features
        .iter()
        .flat_map(|value| value.split(|c: char| c == ',' || c.is_whitespace()))
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
        .collect();
    if !features.is_empty() {
        command.features(CargoOpt::SomeFeatures(features));
    }
    if let Some(target) = &options.target {
        command.other_options(vec!["--filter-platform".into(), target.clone()]);
    }
    command
}

/// Metadata's packages list can include inactive packages. Walk the resolved
/// edges from every workspace member, keeping normal/build/dev dependencies.
/// This is a package graph, not Cargo's exact compilation-unit graph.
pub fn resolved_packages(metadata: &Metadata) -> eyre::Result<Vec<&Package>> {
    let resolve = metadata
        .resolve
        .as_ref()
        .context("Cargo metadata has no resolved dependency graph")?;
    let nodes: BTreeMap<_, _> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.repr.as_str(), node))
        .collect();
    let packages: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .map(|package| (package.id.repr.as_str(), package))
        .collect();
    let mut pending: Vec<_> = metadata.workspace_members.iter().collect();
    let mut visited = BTreeSet::new();
    let mut result = Vec::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id.repr.as_str()) {
            continue;
        }
        let node = nodes
            .get(id.repr.as_str())
            .with_context(|| format!("missing resolve node for {id}"))?;
        let package = packages
            .get(id.repr.as_str())
            .with_context(|| format!("missing package metadata for {id}"))?;
        result.push(*package);
        pending.extend(node.deps.iter().map(|dependency| &dependency.pkg));
    }
    result.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.id.repr.cmp(&b.id.repr))
    });
    Ok(result)
}

pub struct ReviewQueue {
    pub packages: Vec<ReviewPackage>,
    pub config: ConfigFile,
}

impl ReviewQueue {
    pub fn from_metadata(metadata: &Metadata) -> eyre::Result<Self> {
        let config = ConfigFile::for_workspace(metadata.workspace_root.as_std_path())?;
        let packages: Vec<_> = resolved_packages(metadata)?
            .into_iter()
            .filter(|package| !crate::workspace::is_member(metadata, package))
            .filter(|package| {
                package
                    .targets
                    .iter()
                    .any(|target| target.is_custom_build())
            })
            .map(|package| ReviewPackage::from_package(package, &config))
            .collect();
        Ok(Self { packages, config })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            r#"
            [package]
            name = "graph-fixture"
            version = "1.0.0"
            [workspace]
            members = ["member"]
            exclude = ["deps/shared", "deps/optional", "deps/default", "deps/unix", "deps/windows", "deps/build-only", "deps/dev-only"]
            [features]
            default = ["dep:default-script"]
            extra = ["dep:optional-script"]
            [dependencies]
            shared = { path = "deps/shared" }
            default-script = { path = "deps/default", optional = true }
            optional-script = { path = "deps/optional", optional = true }
            [build-dependencies]
            shared = { path = "deps/shared" }
            build-only = { path = "deps/build-only" }
            [dev-dependencies]
            dev-only = { path = "deps/dev-only" }
            [target.'cfg(unix)'.dependencies]
            unix-script = { path = "deps/unix" }
            [target.'cfg(windows)'.dependencies]
            windows-script = { path = "deps/windows" }
        "#,
        );
        write(root.path(), "src/lib.rs", "");
        write(
            root.path(),
            "member/Cargo.toml",
            "[package]\nname='member'\nversion='1.0.0'\n",
        );
        write(root.path(), "member/src/lib.rs", "");
        write(
            root.path(),
            "member/build.rs",
            "compile_error!(\"must not compile\");",
        );
        for (dir, name) in [
            ("shared", "shared"),
            ("optional", "optional-script"),
            ("default", "default-script"),
            ("unix", "unix-script"),
            ("windows", "windows-script"),
            ("build-only", "build-only"),
            ("dev-only", "dev-only"),
        ] {
            write(
                root.path(),
                &format!("deps/{dir}/Cargo.toml"),
                &format!("[package]\nname='{name}'\nversion='1.0.0'\n[workspace]\n"),
            );
            write(root.path(), &format!("deps/{dir}/src/lib.rs"), "");
            write(
                root.path(),
                &format!("deps/{dir}/build.rs"),
                "compile_error!(\"must not compile\");",
            );
        }
        root
    }

    fn queue(root: &Path, options: &ReviewOptions) -> ReviewQueue {
        let mut command = metadata_command(options);
        command.manifest_path(root.join("Cargo.toml"));
        let metadata = command.exec().unwrap();
        ReviewQueue::from_metadata(&metadata).unwrap()
    }

    #[test]
    fn discovers_active_features_and_all_dependency_kinds_once() {
        let root = fixture();
        let default = queue(root.path(), &ReviewOptions::default());
        let names: Vec<_> = default.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "build-only",
                "default-script",
                "dev-only",
                "shared",
                "unix-script",
                "windows-script"
            ]
        );
        let selected = queue(
            root.path(),
            &ReviewOptions {
                no_default_features: true,
                features: vec!["extra".into()],
                ..Default::default()
            },
        );
        assert!(
            selected
                .packages
                .iter()
                .any(|p| p.name == "optional-script")
        );
        assert!(!selected.packages.iter().any(|p| p.name == "default-script"));
        let all = queue(
            root.path(),
            &ReviewOptions {
                all_features: true,
                ..Default::default()
            },
        );
        assert_eq!(all.packages.len(), 7);
        assert!(!root.path().join("approved-builds.toml").exists());
        assert!(!root.path().join("target").exists());
    }

    #[test]
    fn filters_target_specific_edges() {
        let root = fixture();
        for (target, included, excluded) in [
            ("x86_64-unknown-linux-gnu", "unix-script", "windows-script"),
            ("x86_64-pc-windows-gnu", "windows-script", "unix-script"),
        ] {
            let queue = queue(
                root.path(),
                &ReviewOptions {
                    target: Some(target.into()),
                    ..Default::default()
                },
            );
            assert!(queue.packages.iter().any(|p| p.name == included));
            assert!(!queue.packages.iter().any(|p| p.name == excluded));
            assert!(queue.packages.iter().any(|p| p.name == "build-only"));
        }
    }

    #[test]
    fn retains_multiple_versions_for_independent_review() {
        let (root, metadata) = super::super::tests::fixture();
        let queue = ReviewQueue::from_metadata(&metadata).unwrap();
        assert_eq!(
            queue
                .packages
                .iter()
                .filter(|p| p.name == "duplicate")
                .count(),
            2
        );
        assert!(!root.path().join("approved-builds.toml").exists());
    }
}
