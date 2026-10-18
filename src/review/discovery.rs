use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use cargo_metadata::{CargoOpt, Metadata, MetadataCommand, Node, NodeDep, Package};
use eyre::ContextCompat;

use super::ReviewPackage;
use crate::{
    args::ReviewOptions,
    config::{self, ApprovalTarget, edit::ConfigFile},
};

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

/// Metadata can expose inactive optional edges through weak feature forwarding.
/// Walk enabled edges from every workspace member, retaining normal/build/dev.
/// This is a package graph, not Cargo's exact compilation-unit graph.
pub fn resolved_packages<'a>(
    metadata: &'a Metadata,
    options: &ReviewOptions,
) -> eyre::Result<Vec<&'a Package>> {
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
    let workspace_names: BTreeSet<_> = metadata
        .packages
        .iter()
        .filter(|package| crate::workspace::is_member(metadata, package))
        .map(|package| package.name.as_str())
        .collect();
    // Metadata omits direct CLI dependency-feature activation from Node.features.
    let explicit_dependencies: BTreeSet<_> = options
        .features
        .iter()
        .flat_map(|value| value.split(|c: char| c == ',' || c.is_whitespace()))
        .filter_map(|feature| feature.split_once('/').map(|(dependency, _)| dependency))
        .filter(|dependency| !workspace_names.contains(dependency))
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
        let mut enabled = enabled_optional_dependencies(package, node);
        if resolve.root.as_ref().map_or_else(
            || metadata.workspace_members.contains(id),
            |root| root == id,
        ) {
            enabled.extend(explicit_dependencies.iter().copied());
        }
        for dependency in &node.deps {
            let target = packages
                .get(dependency.pkg.repr.as_str())
                .with_context(|| format!("missing package metadata for {}", dependency.pkg))?;
            if dependency_is_active(package, dependency, target, &enabled)? {
                pending.push(&dependency.pkg);
            }
        }
    }
    result.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.id.repr.cmp(&b.id.repr))
    });
    Ok(result)
}

fn enabled_optional_dependencies<'a>(package: &'a Package, node: &'a Node) -> BTreeSet<&'a str> {
    node.features
        .iter()
        .flat_map(|feature| package.features.get(feature.as_str()).into_iter().flatten())
        .map(String::as_str)
        .chain(node.features.iter().map(|feature| feature.as_str()))
        .filter_map(|feature| {
            feature.strip_prefix("dep:").or_else(|| {
                feature
                    .split_once('/')
                    .map(|(dependency, _)| dependency)
                    .filter(|dependency| !dependency.ends_with('?'))
            })
        })
        .collect()
}

fn dependency_is_active(
    package: &Package,
    edge: &NodeDep,
    target: &Package,
    enabled: &BTreeSet<&str>,
) -> eyre::Result<bool> {
    let matches_name = |name: &str| {
        name.bytes()
            .map(|byte| if byte == b'-' { b'_' } else { byte })
            .eq(edge.name.bytes())
    };
    let mut declared = false;
    for dependency in &package.dependencies {
        if dependency.name != target.name.as_str() || !dependency.req.matches(&target.version) {
            continue;
        }
        let key = dependency.rename.as_deref().unwrap_or(&dependency.name);
        if !matches_name(key)
            && !(dependency.rename.is_none()
                && target
                    .targets
                    .iter()
                    .any(|target| matches_name(&target.name)))
        {
            continue;
        }
        declared = true;
        if edge
            .dep_kinds
            .iter()
            .any(|kind| kind.kind == dependency.kind && kind.target == dependency.target)
            && (!dependency.optional || enabled.contains(key))
        {
            return Ok(true);
        }
    }
    eyre::ensure!(
        declared,
        "could not match resolved dependency {} to a declaration in {}",
        edge.name,
        package.name
    );
    Ok(false)
}

pub struct ReviewQueue {
    pub packages: Vec<ReviewPackage>,
    pub config: ConfigFile,
    pub build_directory: PathBuf,
    pub workspace_root: PathBuf,
    pub policy_notes: Vec<String>,
    pub global_target: Option<ApprovalTarget>,
}

impl ReviewQueue {
    pub fn from_metadata(
        metadata: &Metadata,
        options: &ReviewOptions,
        global_path: Option<&Path>,
    ) -> eyre::Result<Self> {
        let workspace_root = metadata.workspace_root.clone().into_std_path_buf();
        let config = ConfigFile::open(config::review_target(
            &workspace_root,
            options.global,
            global_path,
        )?)?;
        let policy_notes = super::policy_notes(&workspace_root, config.target(), global_path)?;
        let packages: Vec<_> = resolved_packages(metadata, options)?
            .into_iter()
            .filter(|package| options.global || !crate::workspace::is_member(metadata, package))
            .filter(|package| {
                package
                    .targets
                    .iter()
                    .any(|target| target.is_custom_build())
            })
            .map(|package| ReviewPackage::from_package(package, &config))
            .collect();
        Ok(Self {
            packages,
            config,
            workspace_root,
            policy_notes,
            global_target: global_path
                .filter(|_| !options.global)
                .map(|path| {
                    config::review_target(metadata.workspace_root.as_std_path(), true, Some(path))
                })
                .transpose()?,
            build_directory: metadata
                .build_directory
                .as_ref()
                .unwrap_or(&metadata.target_directory)
                .clone()
                .into_std_path_buf(),
        })
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
        ReviewQueue::from_metadata(&metadata, options, None).unwrap()
    }

    #[test]
    fn global_review_includes_members_and_uses_only_selected_store() {
        let root = fixture();
        let external = tempfile::tempdir().unwrap();
        let global = external.path().join("approved-builds.toml");
        fs::write(&global, "[approved]\nmember='1.0.0'\nshared='1.0.0'\n").unwrap();
        write(
            root.path(),
            "approved-builds.toml",
            "[approved]\nbuild-only='1.0.0'\n",
        );
        let metadata = metadata_command(&ReviewOptions::default())
            .manifest_path(root.path().join("Cargo.toml"))
            .exec()
            .unwrap();
        let options = ReviewOptions {
            global: true,
            ..Default::default()
        };
        let global_queue = ReviewQueue::from_metadata(&metadata, &options, Some(&global)).unwrap();
        assert_eq!(
            global_queue.workspace_root,
            metadata.workspace_root.as_std_path()
        );
        assert_eq!(global_queue.config.path(), global);
        assert!(
            global_queue
                .packages
                .iter()
                .any(|p| p.name == "member" && p.approval.is_some())
        );
        assert!(
            global_queue
                .packages
                .iter()
                .any(|p| p.name == "build-only" && p.approval.is_none())
        );
        let member = super::super::ReviewSession::from_metadata(
            &metadata,
            "member@1.0.0",
            &options,
            Some(&global),
        )
        .unwrap();
        assert_eq!(member.package.name, "member");
        assert_eq!(member.workspace_root, global_queue.workspace_root);
        assert_eq!(member.policy_notes, global_queue.policy_notes);
        let workspace_queue =
            ReviewQueue::from_metadata(&metadata, &ReviewOptions::default(), Some(&global))
                .unwrap();
        assert!(!workspace_queue.packages.iter().any(|p| p.name == "member"));
        assert!(
            workspace_queue
                .packages
                .iter()
                .any(|p| p.name == "shared" && p.approval.is_none())
        );
        assert!(
            workspace_queue
                .packages
                .iter()
                .any(|p| p.name == "build-only" && p.approval.is_some())
        );
        assert!(
            super::super::ReviewSession::from_metadata(
                &metadata,
                "member",
                &ReviewOptions::default(),
                Some(&global)
            )
            .is_err()
        );
        let dependency = super::super::ReviewSession::from_metadata(
            &metadata,
            "shared",
            &ReviewOptions::default(),
            Some(&global),
        )
        .unwrap();
        assert!(dependency.package.approval.is_none());
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
    fn weak_feature_forwarding_does_not_activate_optional_backend_or_its_dependencies() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            "[package]\nname='consumer'\nversion='1.0.0'\n\
             [workspace]\nexclude=['backend','leaf']\n\
             [features]\ndefault=['style']\nstyle=['backend-alias?/color']\n\
             backend=['dep:backend-alias']\nstrong=['backend-alias/color']\n\
             [dependencies]\nbackend-alias={package='backend',path='backend',optional=true}\n",
        );
        write(root.path(), "src/lib.rs", "");
        write(
            root.path(),
            "backend/Cargo.toml",
            "[package]\nname='backend'\nversion='1.0.0'\n[workspace]\n\
             [features]\ncolor=[]\n[dependencies]\nleaf={path='../leaf'}\n",
        );
        write(
            root.path(),
            "leaf/Cargo.toml",
            "[package]\nname='leaf'\nversion='1.0.0'\n[workspace]\n[lib]\nname='custom_leaf'\n",
        );
        for package in ["backend", "leaf"] {
            write(root.path(), &format!("{package}/src/lib.rs"), "");
            write(
                root.path(),
                &format!("{package}/build.rs"),
                "compile_error!(\"review must not build scripts\");",
            );
        }
        let metadata = metadata_command(&ReviewOptions::default())
            .manifest_path(root.path().join("Cargo.toml"))
            .exec()
            .unwrap();
        let inactive =
            ReviewQueue::from_metadata(&metadata, &crate::args::ReviewOptions::default(), None)
                .unwrap();
        assert!(
            super::super::ReviewSession::from_metadata(
                &metadata,
                "leaf",
                &ReviewOptions::default(),
                None,
            )
            .is_err()
        );
        assert!(
            inactive.packages.is_empty(),
            "weak forwarding activated an unused backend: {:?}",
            inactive.packages
        );
        for feature in ["backend", "strong", "backend-alias/color"] {
            let options = ReviewOptions {
                features: vec![feature.into()],
                ..Default::default()
            };
            let metadata = metadata_command(&options)
                .manifest_path(root.path().join("Cargo.toml"))
                .exec()
                .unwrap();
            let active = ReviewQueue::from_metadata(&metadata, &options, None).unwrap();
            let selected =
                super::super::ReviewSession::from_metadata(&metadata, "leaf", &options, None)
                    .unwrap();
            assert_eq!(selected.package.name, "leaf");
            let names: Vec<_> = active
                .packages
                .iter()
                .map(|package| package.name.as_str())
                .collect();
            assert_eq!(names, ["backend", "leaf"]);
        }
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
        let queue =
            ReviewQueue::from_metadata(&metadata, &crate::args::ReviewOptions::default(), None)
                .unwrap();
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
