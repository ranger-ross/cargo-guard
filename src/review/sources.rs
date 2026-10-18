use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use cargo_metadata::{Metadata, MetadataCommand, Package};
use eyre::{Context, ContextCompat};
use sha2::{Digest, Sha256};

use super::ReviewSession;
use crate::{
    config::ApprovedVersion,
    fingerprint::{self, ParsedFingerprint},
};

#[derive(Debug)]
pub struct PackageSources {
    pub version: semver::Version,
    pub scripts: Vec<ScriptSource>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScriptSource {
    pub path: PathBuf,
    pub contents: String,
}

#[derive(Debug)]
pub enum Baseline {
    NewPackage,
    CurrentVersion,
    CurrentFingerprint(Arc<[ScriptSource]>),
    Fingerprint,
    Loading(semver::Version),
    Ready(PackageSources),
    Unavailable {
        version: semver::Version,
        reason: String,
    },
}

impl Baseline {
    pub fn for_session(session: &ReviewSession) -> Self {
        match &session
            .package
            .approval
            .as_ref()
            .and_then(|a| a.for_review(&session.package.version))
        {
            Some(ApprovedVersion::Multiple(_)) => {
                unreachable!("review selects a representative approval")
            }
            None => Self::NewPackage,
            Some(ApprovedVersion::Fingerprint(fingerprint)) => {
                ParsedFingerprint::parse(fingerprint)
                    .map(|parsed| Self::Loading(parsed.version))
                    .unwrap_or(Self::Fingerprint)
            }
            Some(ApprovedVersion::SemVer(version)) if version == &session.package.version => {
                Self::CurrentVersion
            }
            Some(ApprovedVersion::SemVer(version)) => Self::Loading(version.clone()),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NewPackage => "Baseline: none (new approval)".into(),
            Self::CurrentVersion => "Baseline: current version is already approved".into(),
            Self::CurrentFingerprint(_) => "Baseline: current build-script fingerprints are already approved".into(),
            Self::Fingerprint => {
                "Baseline unavailable: fingerprint has no valid version hint; approve a fresh fingerprint".into()
            }
            Self::Loading(version) => format!("Baseline: loading approved v{version}…"),
            Self::Ready(sources) => format!(
                "Baseline: approved v{} loaded ({} build scripts)",
                sources.version,
                sources.scripts.len()
            ),
            Self::Unavailable { version, reason } => {
                format!("Baseline unavailable for approved v{version}: {reason}")
            }
        }
    }
}

pub struct BaselineLoader {
    receiver: mpsc::Receiver<Baseline>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl BaselineLoader {
    pub fn start(session: &ReviewSession) -> eyre::Result<Option<Self>> {
        let Baseline::Loading(version) = Baseline::for_session(session) else {
            return Ok(None);
        };
        // Keep every matching saved fingerprint to authenticate historical
        // sources, including all build scripts and compiled feature variants.
        let approvals = session.config.config().approved.get(&session.package.name);
        let request = SourceRequest {
            name: session.package.name.clone(),
            version: version.clone(),
            source: session
                .package
                .source
                .as_ref()
                .map(|source| source.repr.clone()),
            workspace_root: session.workspace_root.clone(),
            fingerprints: match session
                .package
                .approval
                .as_ref()
                .and_then(|approval| approval.for_review(&session.package.version))
            {
                Some(ApprovedVersion::Fingerprint(_)) => Some(
                    approvals
                        .into_iter()
                        .flat_map(ApprovedVersion::entries)
                        .filter_map(|approval| match approval {
                            ApprovedVersion::Fingerprint(value)
                                if ParsedFingerprint::parse(value)
                                    .is_some_and(|parsed| parsed.version == version) =>
                            {
                                Some(value.clone())
                            }
                            _ => None,
                        })
                        .collect(),
                ),
                _ => None,
            },
        };
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let owner = thread::current();
        let worker = thread::Builder::new()
            .name("approved-sources".into())
            .spawn(move || {
                // Current-artifact verification belongs to the queue worker;
                // this independently cancellable worker only retrieves history.
                if worker_cancelled.load(Ordering::Relaxed) {
                    return;
                }
                let baseline = match load_sources(&request, &worker_cancelled) {
                    Ok(sources) => Baseline::Ready(sources),
                    Err(error) => Baseline::Unavailable {
                        version,
                        reason: format!("{error:#}"),
                    },
                };
                let _ = sender.send(baseline);
                owner.unpark();
            })
            .context("could not start baseline source loader")?;
        Ok(Some(Self {
            receiver,
            cancelled,
            worker: Some(worker),
        }))
    }

    pub fn poll(&self) -> Option<Baseline> {
        self.receiver.try_recv().ok()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for BaselineLoader {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct SourceRequest {
    name: String,
    version: semver::Version,
    source: Option<String>,
    workspace_root: PathBuf,
    // None preserves ordinary version-based approvals. Some requires every
    // historical script to match a saved fingerprint before exposing sources.
    fingerprints: Option<Vec<String>>,
}

/// Load source text without compiling the package or executing build scripts.
fn load_sources(request: &SourceRequest, cancelled: &AtomicBool) -> eyre::Result<PackageSources> {
    let index = registry_index(request.source.as_deref())?;
    let temporary = tempfile::tempdir().context("could not create baseline workspace")?;
    let manifest_path = temporary.path().join("Cargo.toml");
    fs::write(&manifest_path, baseline_manifest(request))?;
    fs::write(temporary.path().join("lib.rs"), "")?;

    let registry = format!(
        "registries.cargo-guard-review.index={}",
        toml::Value::String(index.to_owned())
    );
    let options = vec![
        "--color".into(),
        "never".into(),
        "--config".into(),
        registry,
    ];
    let mut command = MetadataCommand::new();
    command
        .manifest_path(&manifest_path)
        .current_dir(&request.workspace_root);
    // Use Cargo's cache and source configuration; do not assume a particular
    // registry cache layout or download/extract unverified archives ourselves.
    let mut offline = options.clone();
    offline.push("--offline".into());
    command.other_options(offline);
    let metadata = match execute_metadata(&command, cancelled) {
        Ok(metadata) => metadata,
        Err(_) if !cancelled.load(Ordering::Relaxed) => {
            command.other_options(options);
            execute_metadata(&command, cancelled)
                .context("could not retrieve the approved release (it may be unavailable or not cached while offline)")?
        }
        Err(error) => return Err(error),
    };
    let package = metadata
        .packages
        .iter()
        .find(|package| {
            package.name.as_str() == request.name
                && package.version == request.version
                && package.source.as_ref().map(|source| source.repr.as_str())
                    == request.source.as_deref()
        })
        .context(
            "Cargo did not return the exact approved package version from the original registry",
        )?;
    let sources = read_package_sources(package)?;
    if let Some(approvals) = &request.fingerprints {
        let mut command = MetadataCommand::new();
        command
            .manifest_path(request.workspace_root.join("Cargo.toml"))
            .current_dir(&request.workspace_root)
            .no_deps()
            .other_options(vec!["--offline".into()]);
        let workspace = execute_metadata(&command, cancelled)
            .context("could not locate existing compiled inputs for fingerprint verification")?;
        let mut artifacts = Vec::new();
        if let Some(directory) = &workspace.build_directory {
            super::fingerprint::collect(directory.as_std_path(), &mut artifacts)?;
        }
        if workspace.build_directory.as_ref() != Some(&workspace.target_directory) {
            super::fingerprint::collect(workspace.target_directory.as_std_path(), &mut artifacts)?;
        }
        let root = package
            .manifest_path
            .parent()
            .context("baseline manifest has no parent")?;
        let targets = package
            .targets
            .iter()
            .filter(|target| target.is_custom_build())
            .map(|target| {
                Ok((
                    target.name.clone(),
                    target.src_path.strip_prefix(root)?.into(),
                ))
            })
            .collect::<eyre::Result<Vec<(String, PathBuf)>>>()?;
        verify_fingerprint_baseline(
            package.manifest_path.as_std_path(),
            &sources,
            &targets,
            approvals,
            &request.workspace_root,
            &artifacts,
            cancelled,
        )?;
    }
    Ok(sources)
}

/// Existing dep-info supplies candidate input sets, never authorization. Only
/// the saved pair of hashes can authenticate a remapped historical package.
fn verify_fingerprint_baseline(
    manifest_path: &Path,
    sources: &PackageSources,
    targets: &[(String, PathBuf)],
    approvals: &[String],
    workspace_root: &Path,
    artifacts: &[PathBuf],
    cancelled: &AtomicBool,
) -> eyre::Result<()> {
    let version = &sources.version;
    eyre::ensure!(
        sources.scripts.len() == targets.len(),
        "baseline source snapshots do not match the build targets"
    );
    let approvals: Vec<_> = approvals
        .iter()
        .filter(|value| {
            ParsedFingerprint::parse(value).is_some_and(|parsed| parsed.version == *version)
        })
        .collect();
    eyre::ensure!(
        !approvals.is_empty(),
        "no saved fingerprints for the requested version hint"
    );
    eyre::ensure!(
        !targets.is_empty(),
        "version hint points to a package without build scripts"
    );
    let mut matched_approvals = vec![false; approvals.len()];
    let version_string = version.to_string();
    let root = manifest_path
        .parent()
        .context("baseline manifest has no parent")?
        .canonicalize()?;
    for (name, script) in targets {
        eyre::ensure!(
            script
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "baseline script path is not package-relative"
        );
        let source = sources
            .scripts
            .iter()
            .find(|source| source.path == *script)
            .context("baseline target has no source snapshot")?;
        let mut snapshot_hash = String::with_capacity(64);
        fingerprint::append_hex(
            &mut snapshot_hash,
            &Sha256::digest(source.contents.as_bytes()),
        );
        let mut verified = false;
        let mut reason =
            "no matching compiled dep-info; build the current package before reviewing".to_owned();
        for artifact in artifacts {
            eyre::ensure!(
                !cancelled.load(Ordering::Relaxed),
                "baseline loading cancelled"
            );
            if !super::fingerprint::matches_target(artifact, name) {
                continue;
            }
            let candidate = (|| -> eyre::Result<()> {
                let compiled = fs::metadata(artifact)?.modified()?;
                let contents = fs::read_to_string(artifact)?;
                let inputs = fingerprint::parse_dep_info(&contents)?
                    .into_iter()
                    .map(|path| workspace_root.join(path))
                    .collect::<Vec<_>>();
                eyre::ensure!(
                    inputs
                        .iter()
                        .all(|path| !path.components().any(|part| part == Component::ParentDir)),
                    "compiled input contains an unmappable parent path"
                );
                // A record may reference the current release or a historical
                // registry directory that Cargo has since removed. Infer its
                // root from the complete relative entry path, not its basename.
                let mut failure =
                    "compiled dep-info does not track the requested entry path".to_owned();
                for entry in inputs.iter().filter(|path| path.ends_with(script)) {
                    let candidate_root = entry
                        .ancestors()
                        .nth(script.components().count())
                        .context("compiled entry path has no package root")?;
                    let mapped = match remap_inputs(&inputs, candidate_root, &root, compiled) {
                        Ok(mapped) => mapped,
                        Err(error) => {
                            failure = format!("{error:#}");
                            continue;
                        }
                    };
                    let fingerprint = match fingerprint::from_dep_info(
                        manifest_path,
                        &mapped,
                        Some(&root),
                        &version_string,
                    ) {
                        Ok(fingerprint) => fingerprint,
                        Err(error) => {
                            failure = format!("{error:#}");
                            continue;
                        }
                    };
                    eyre::ensure!(
                        ParsedFingerprint::parse(&fingerprint)
                            .is_some_and(|parsed| parsed.script_hash == snapshot_hash),
                        "baseline source snapshot does not match the verified script hash"
                    );
                    eyre::ensure!(
                        fs::read_to_string(artifact)? == contents,
                        "compiled dep-info changed during baseline verification"
                    );
                    let mut matches = false;
                    for (index, saved) in approvals.iter().enumerate() {
                        if fingerprint::fingerprints_match(saved, &fingerprint) {
                            matched_approvals[index] = true;
                            matches = true;
                        }
                    }
                    if matches {
                        return Ok(());
                    }
                    failure = "both saved fingerprint hashes did not match the retrieved sources and build dependencies".to_owned();
                }
                eyre::bail!("{failure}")
            })();
            match candidate {
                Ok(()) => {
                    verified = true;
                }
                Err(error) => reason = format!("{error:#}"),
            }
        }
        eyre::ensure!(
            verified,
            "untrusted version hint v{version}: could not verify {}: {reason}",
            script.display()
        );
    }
    eyre::ensure!(
        matched_approvals.iter().all(|matched| *matched),
        "untrusted version hint v{version}: not every saved fingerprint could be verified; a script may be missing or compiled feature inputs unavailable"
    );
    Ok(())
}

fn remap_inputs(
    inputs: &[PathBuf],
    candidate_root: &Path,
    baseline_root: &Path,
    compiled: std::time::SystemTime,
) -> eyre::Result<String> {
    let mut dep_info = String::from("verified-baseline:");
    let original_root = candidate_root
        .try_exists()?
        .then(|| candidate_root.canonicalize())
        .transpose()?;
    let manifest = candidate_root.join("Cargo.toml");
    if manifest.try_exists()? {
        eyre::ensure!(
            fs::metadata(manifest)?.modified()? <= compiled,
            "package manifest is newer than compiled dep-info"
        );
    }
    for input in inputs {
        let relative = input
            .strip_prefix(candidate_root)
            .context("compiled input is external to the package")?;
        eyre::ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "compiled input cannot be mapped into the baseline package"
        );
        // When original files still exist, reject stale compilation records.
        // Deleted historical package roots are fine: only retrieved files are
        // hashed, and missing retrieved inputs always fail below.
        if let Some(original_root) = &original_root {
            eyre::ensure!(
                input
                    .canonicalize()
                    .with_context(|| format!("missing original tracked input {}", input.display()))?
                    .starts_with(original_root),
                "compiled input resolves outside the package"
            );
            eyre::ensure!(
                fs::metadata(input)?.modified()? <= compiled,
                "compiled dep-info is stale"
            );
        }
        let mapped = baseline_root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("missing baseline tracked input {}", relative.display()))?;
        eyre::ensure!(
            mapped.starts_with(baseline_root),
            "baseline input resolves outside the package"
        );
        let path = relative
            .to_str()
            .context("compiled input path is not UTF-8")?;
        eyre::ensure!(
            !path.contains(['\n', '\r']),
            "compiled input path contains a newline"
        );
        dep_info.push(' ');
        for character in path.chars() {
            match character {
                '$' => dep_info.push_str("$$"),
                '\\' | ' ' | '\t' | '#' | ':' => {
                    dep_info.push('\\');
                    dep_info.push(character);
                }
                _ => dep_info.push(character),
            }
        }
    }
    dep_info.push('\n');
    Ok(dep_info)
}

fn registry_index(source: Option<&str>) -> eyre::Result<&str> {
    match source {
        Some(source) if source.starts_with("registry+") => Ok(&source["registry+".len()..]),
        Some(source) if source.starts_with("sparse+") => Ok(source),
        _ => eyre::bail!(
            "historical sources for Git/path packages cannot be identified by version alone"
        ),
    }
}

fn baseline_manifest(request: &SourceRequest) -> String {
    format!(
        "[package]\nname = 'cargo-guard-source-review'\nversion = '0.0.0'\nedition = '2024'\n\
         [workspace]\n\
         [lib]\npath = 'lib.rs'\n\
         [dependencies]\nreviewed-package = {{ package = {}, version = {}, registry = 'cargo-guard-review', default-features = false }}\n",
        toml::Value::String(request.name.clone()),
        toml::Value::String(format!("={}", request.version)),
    )
}

fn read_package_sources(package: &Package) -> eyre::Result<PackageSources> {
    let root = package
        .manifest_path
        .parent()
        .context("baseline manifest has no parent")?;
    let mut scripts = Vec::new();
    for target in package
        .targets
        .iter()
        .filter(|target| target.is_custom_build())
    {
        let path = target
            .src_path
            .strip_prefix(root)
            .context("baseline build script is outside its package directory")?;
        let contents = fs::read_to_string(&target.src_path)
            .with_context(|| format!("could not read approved build script {}", target.src_path))?;
        scripts.push(ScriptSource {
            path: path.into(),
            contents,
        });
    }
    scripts.sort_by(|a, b| a.path.cmp(&b.path));
    // An old package may legitimately have had no build scripts at all.
    Ok(PackageSources {
        version: package.version.clone(),
        scripts,
    })
}

fn execute_metadata(command: &MetadataCommand, cancelled: &AtomicBool) -> eyre::Result<Metadata> {
    eyre::ensure!(
        !cancelled.load(Ordering::Relaxed),
        "baseline loading cancelled"
    );
    // File-backed output avoids pipe deadlocks and keeps Cargo's progress output
    // off the TUI. Poll the child so quitting can cancel a download promptly.
    let stdout = tempfile::NamedTempFile::new()?;
    let stderr = tempfile::NamedTempFile::new()?;
    let mut child = command
        .cargo_command()
        .stdin(Stdio::null())
        .stdout(stdout.reopen()?)
        .stderr(stderr.reopen()?)
        .spawn()
        .context("could not start Cargo to retrieve approved sources")?;
    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            eyre::bail!("baseline loading cancelled");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("could not wait for baseline source retrieval");
            }
        }
    };
    eyre::ensure!(
        status.success(),
        "Cargo source retrieval failed: {}",
        fs::read_to_string(stderr.path())?.trim()
    );
    MetadataCommand::parse(fs::read_to_string(stdout.path())?)
        .context("could not parse Cargo metadata for the approved release")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn global_history_uses_the_consuming_workspaces_source_configuration() {
        use crate::config::{ApprovalScope, ApprovalTarget, edit::ConfigFile};

        let (root, mut app) = super::super::app::tests::fixture(&["fn main() {}"]);
        let global = tempfile::tempdir().unwrap();
        app.session.config = ConfigFile::open(ApprovalTarget {
            path: global.path().join("approved-builds.toml"),
            scope: ApprovalScope::Global,
        })
        .unwrap();
        let historical = root.path().join("vendor/example");
        fs::create_dir_all(historical.join("src")).unwrap();
        fs::write(
            historical.join("Cargo.toml"),
            "[package]\nname='example'\nversion='0.0.1'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(historical.join("src/lib.rs"), "").unwrap();
        let contents = "compile_error!(\"historical scripts must not execute\");\n";
        fs::write(historical.join("build.rs"), contents).unwrap();
        fs::write(
            historical.join(".cargo-checksum.json"),
            r#"{"files":{},"package":null}"#,
        )
        .unwrap();
        fs::create_dir(root.path().join(".cargo")).unwrap();
        fs::write(
            root.path().join(".cargo/config.toml"),
            format!(
                "[net]\noffline=true\n[source.crates-io]\nreplace-with='fixture'\n\
                 [source.fixture]\ndirectory={}\n",
                toml::Value::String(root.path().join("vendor").display().to_string())
            ),
        )
        .unwrap();
        app.session.package.source = Some(cargo_metadata::Source {
            repr: "registry+https://github.com/rust-lang/crates.io-index".into(),
        });
        app.session.package.approval = Some(ApprovedVersion::SemVer("0.0.1".parse().unwrap()));
        let loader = BaselineLoader::start(&app.session).unwrap().unwrap();
        let result = loader
            .receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        let Baseline::Ready(sources) = result else {
            panic!("expected historical sources, got {result:?}");
        };
        assert_eq!(sources.version, "0.0.1".parse::<semver::Version>().unwrap());
        assert_eq!(sources.scripts.len(), 1);
        assert_eq!(sources.scripts[0].path, Path::new("build.rs"));
        assert_eq!(sources.scripts[0].contents, contents);
        assert!(!global.path().join("target").exists());
        assert!(!app.session.config.path().exists());
    }

    #[test]
    fn skips_unneeded_loads_and_reports_unsupported_history() {
        let (_root, mut app) = super::super::app::tests::fixture(&["fn main() {}"]);
        assert!(BaselineLoader::start(&app.session).unwrap().is_none());
        app.session.package.approval =
            Some(ApprovedVersion::SemVer(app.session.package.version.clone()));
        assert!(BaselineLoader::start(&app.session).unwrap().is_none());
        app.session.package.approval = Some(ApprovedVersion::Fingerprint("hash".into()));
        assert!(BaselineLoader::start(&app.session).unwrap().is_none());
        app.session.package.approval = Some(ApprovedVersion::SemVer("0.0.1".parse().unwrap()));
        let loader = BaselineLoader::start(&app.session).unwrap().unwrap();
        let result = loader
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(
            matches!(result, Baseline::Unavailable { reason, .. } if reason.contains("Git/path"))
        );
        assert!(!app.session.config.path().exists());
    }

    #[test]
    fn reports_unavailable_release_while_honoring_offline_config() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".cargo")).unwrap();
        fs::write(
            root.path().join(".cargo/config.toml"),
            "[net]\noffline = true\n",
        )
        .unwrap();
        let request = SourceRequest {
            name: "missing-cargo-guard-fixture".into(),
            version: "1.0.0".parse().unwrap(),
            source: Some("sparse+http://127.0.0.1:9/cargo-guard-test-index/".into()),
            workspace_root: root.path().to_path_buf(),
            fingerprints: None,
        };
        let error = load_sources(&request, &AtomicBool::new(false)).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("could not retrieve the approved release"),
            "{message}"
        );
        assert!(message.contains("offline"), "{message}");
        assert!(!root.path().join("Cargo.lock").exists());
    }

    #[test]
    fn pins_exact_version_in_an_isolated_workspace() {
        let request = SourceRequest {
            name: "example".into(),
            version: "1.2.3".parse().unwrap(),
            source: None,
            workspace_root: PathBuf::new(),
            fingerprints: None,
        };
        let manifest: toml::Value = toml::from_str(&baseline_manifest(&request)).unwrap();
        assert!(manifest.get("workspace").is_some());
        assert_eq!(
            manifest["dependencies"]["reviewed-package"]["version"].as_str(),
            Some("=1.2.3")
        );
        assert_eq!(
            manifest["dependencies"]["reviewed-package"]["package"].as_str(),
            Some("example")
        );
        assert_eq!(
            registry_index(Some("registry+https://example.test/index")).unwrap(),
            "https://example.test/index"
        );
        assert_eq!(
            registry_index(Some("sparse+https://example.test/index/")).unwrap(),
            "sparse+https://example.test/index/"
        );
        assert!(registry_index(None).is_err());
        assert!(registry_index(Some("git+https://example.test/repo")).is_err());
    }

    #[test]
    fn reads_overridden_multiple_and_missing_baseline_scripts_without_building() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='example'\nversion='1.0.0'\nbuild='custom.rs'\n[workspace]\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/lib.rs"), "").unwrap();
        let code = "compile_error!(\"must not compile\");";
        fs::write(root.path().join("custom.rs"), code).unwrap();
        let metadata = MetadataCommand::new()
            .manifest_path(root.path().join("Cargo.toml"))
            .no_deps()
            .exec()
            .unwrap();
        let mut package = metadata.packages[0].clone();
        let sources = read_package_sources(&package).unwrap();
        assert_eq!(sources.scripts[0].path, Path::new("custom.rs"));
        assert_eq!(sources.scripts[0].contents, code);
        let mut second = package
            .targets
            .iter()
            .find(|target| target.is_custom_build())
            .unwrap()
            .clone();
        second.src_path = second.src_path.parent().unwrap().join("second.rs");
        fs::write(second.src_path.as_std_path(), "// second").unwrap();
        package.targets.push(second);
        assert_eq!(read_package_sources(&package).unwrap().scripts.len(), 2);
        fs::remove_file(root.path().join("second.rs")).unwrap();
        assert!(read_package_sources(&package).is_err());
        package.targets.retain(|target| !target.is_custom_build());
        assert!(read_package_sources(&package).unwrap().scripts.is_empty());
        assert!(!root.path().join("target").exists());
    }

    struct VerifiedFixture {
        directory: tempfile::TempDir,
        manifest: PathBuf,
        version: semver::Version,
        targets: Vec<(String, PathBuf)>,
        approvals: Vec<String>,
        artifacts: Vec<PathBuf>,
    }

    impl VerifiedFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let baseline = directory.path().join("baseline");
            fs::create_dir_all(baseline.join("scripts")).unwrap();
            let manifest = baseline.join("Cargo.toml");
            fs::write(
                &manifest,
                "[package]\nname='example'\nversion='1.0.0'\nbuild=['scripts/first.rs','scripts/second.rs']\n[build-dependencies]\ncc='1'\n",
            ).unwrap();
            fs::write(
                baseline.join("scripts/first.rs"),
                "compile_error!(\"never execute\");",
            )
            .unwrap();
            fs::write(
                baseline.join("scripts/second.rs"),
                "compile_error!(\"also never execute\");",
            )
            .unwrap();
            fs::write(baseline.join("helper file.rs"), "// approved helper").unwrap();
            let targets = vec![
                (
                    "build-script-first".to_owned(),
                    PathBuf::from("scripts/first.rs"),
                ),
                (
                    "build-script-second".to_owned(),
                    PathBuf::from("scripts/second.rs"),
                ),
            ];
            let mut approvals = Vec::new();
            let mut artifacts = Vec::new();
            // The historical source tree is deliberately absent. Its dep-info
            // still describes usable package-relative inputs.
            let historical = directory.path().join("example-0.9.0");
            for (name, entry) in &targets {
                let dep_info = format!("binary: {} helper\\ file.rs\n", entry.display());
                approvals.push(
                    fingerprint::from_dep_info(&manifest, &dep_info, Some(&baseline), "1.0.0")
                        .unwrap(),
                );
                let artifact = directory
                    .path()
                    .join(format!("{}-abcd.d", name.replace('-', "_")));
                fs::write(
                    &artifact,
                    format!(
                        "binary: {} {}/helper\\ file.rs\n",
                        historical.join(entry).display(),
                        historical.display(),
                    ),
                )
                .unwrap();
                artifacts.push(artifact);
            }
            Self {
                directory,
                manifest,
                version: "1.0.0".parse().unwrap(),
                targets,
                approvals,
                artifacts,
            }
        }

        fn sources(&self) -> eyre::Result<PackageSources> {
            Ok(PackageSources {
                version: self.version.clone(),
                scripts: self
                    .targets
                    .iter()
                    .map(|(_, path)| {
                        Ok(ScriptSource {
                            path: path.clone(),
                            contents: fs::read_to_string(
                                self.manifest.parent().unwrap().join(path),
                            )?,
                        })
                    })
                    .collect::<eyre::Result<Vec<_>>>()?,
            })
        }

        fn verify(&self) -> eyre::Result<()> {
            verify_fingerprint_baseline(
                &self.manifest,
                &self.sources()?,
                &self.targets,
                &self.approvals,
                self.directory.path(),
                &self.artifacts,
                &AtomicBool::new(false),
            )
        }
    }

    #[test]
    fn fingerprint_baseline_verifies_all_scripts_from_historical_or_current_inputs() {
        let mut fixture = VerifiedFixture::new();
        fixture.verify().unwrap();
        // Current compiled paths are useful too; only retrieved baseline bytes
        // are hashed, not current source contents.
        let current = fixture.directory.path().join("current");
        fs::create_dir_all(current.join("scripts")).unwrap();
        fs::write(
            current.join("scripts/first.rs"),
            "// current release changed",
        )
        .unwrap();
        fs::write(current.join("helper file.rs"), "// current helper changed").unwrap();
        fs::write(
            &fixture.artifacts[0],
            "binary: current/scripts/first.rs current/helper\\ file.rs\n",
        )
        .unwrap();
        fixture.verify().unwrap();
        fixture.approvals.pop();
        assert!(
            fixture
                .verify()
                .unwrap_err()
                .to_string()
                .contains("second.rs")
        );
        assert!(!fixture.directory.path().join("target").exists());
    }

    #[test]
    fn fingerprint_baseline_rejects_forged_hints_and_either_hash_mismatch() {
        let mut fixture = VerifiedFixture::new();
        let saved = fixture.approvals.clone();
        fixture.version = "9.9.9".parse().unwrap();
        assert!(fixture.verify().is_err());
        fixture.approvals = saved
            .iter()
            .map(|value| value.replace("#1.0.0", "#9.9.9"))
            .collect();
        // A rewritten hint can only be accepted when the retrieved source
        // contents really match both hashes. Here the hinted release differs.
        fs::write(
            fixture.manifest.parent().unwrap().join("scripts/first.rs"),
            "// forged release",
        )
        .unwrap();
        assert!(fixture.verify().is_err());
        fixture.version = "1.0.0".parse().unwrap();
        fixture.approvals = saved.clone();
        fs::write(
            fixture.manifest.parent().unwrap().join("scripts/first.rs"),
            "compile_error!(\"never execute\");",
        )
        .unwrap();
        for index in [0, 65] {
            let replacement = if &saved[0][index..index + 1] == "0" {
                "1"
            } else {
                "0"
            };
            fixture.approvals[0].replace_range(index..index + 1, replacement);
            assert!(fixture.verify().is_err());
            fixture.approvals = saved.clone();
        }
    }

    #[test]
    fn fingerprint_baseline_rejects_changed_helpers_dependencies_and_missing_inputs() {
        let fixture = VerifiedFixture::new();
        let helper = fixture.manifest.parent().unwrap().join("helper file.rs");
        fs::write(&helper, "// modified helper").unwrap();
        assert!(fixture.verify().is_err());
        fs::remove_file(&helper).unwrap();
        assert!(fixture.verify().is_err());
        fs::write(&helper, "// approved helper").unwrap();
        fs::write(&fixture.manifest,
            "[package]\nname='example'\nversion='1.0.0'\nbuild=['scripts/first.rs','scripts/second.rs']\n[build-dependencies]\ncc='2'\n",
        ).unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn fingerprint_baseline_rejects_absent_stale_external_or_wrong_target_records() {
        let mut fixture = VerifiedFixture::new();
        let artifacts = fixture.artifacts.clone();
        fixture.artifacts.clear();
        assert!(
            fixture
                .verify()
                .unwrap_err()
                .to_string()
                .contains("no matching compiled")
        );
        fixture.artifacts = artifacts;
        fs::write(
            &fixture.artifacts[0],
            "binary: baseline/scripts/first.rs baseline/helper\\ file.rs\n",
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(&fixture.artifacts[0])
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() - Duration::from_secs(60)),
            )
            .unwrap();
        assert!(fixture.verify().is_err());
        fs::write(
            &fixture.artifacts[0],
            "binary: baseline/scripts/first.rs outside.rs\n",
        )
        .unwrap();
        assert!(fixture.verify().is_err());
        fs::write(
            &fixture.artifacts[0],
            "binary: baseline/scripts/first.rs baseline/helper\\ file.rs\n",
        )
        .unwrap();
        fixture.targets[0].0 = "different-target".into();
        assert!(fixture.verify().is_err());
        fixture.targets[0].0 = "build-script-first".into();
        fixture.targets[0].1 = "different/first.rs".into();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn fingerprint_baseline_rejects_removed_scripts_and_unverified_source_snapshots() {
        let mut fixture = VerifiedFixture::new();
        let mut sources = fixture.sources().unwrap();
        sources.scripts[0].contents = "// not the verified bytes".into();
        let error = verify_fingerprint_baseline(
            &fixture.manifest,
            &sources,
            &fixture.targets,
            &fixture.approvals,
            fixture.directory.path(),
            &fixture.artifacts,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("snapshot"));
        fixture.targets.pop();
        assert!(
            fixture
                .verify()
                .unwrap_err()
                .to_string()
                .contains("not every saved fingerprint")
        );
    }

    #[test]
    fn fingerprint_baseline_requires_artifacts_for_every_saved_feature_variant() {
        let mut fixture = VerifiedFixture::new();
        let baseline = fixture.manifest.parent().unwrap();
        fs::write(baseline.join("feature.rs"), "// feature input").unwrap();
        fixture.approvals.push(
            fingerprint::from_dep_info(
                &fixture.manifest,
                "binary: scripts/first.rs helper\\ file.rs feature.rs\n",
                Some(baseline),
                "1.0.0",
            )
            .unwrap(),
        );
        assert!(
            fixture
                .verify()
                .unwrap_err()
                .to_string()
                .contains("not every saved fingerprint")
        );
        let artifact = fixture.directory.path().join("build_script_first-ffff.d");
        fs::write(&artifact,
            "binary: example-0.9.0/scripts/first.rs example-0.9.0/helper\\ file.rs example-0.9.0/feature.rs\n",
        ).unwrap();
        fixture.artifacts.push(artifact);
        fixture.verify().unwrap();
    }

    #[test]
    fn valid_fingerprint_hint_loads_but_never_immediately_trusts_current_version() {
        let (_root, mut app) = super::super::app::tests::fixture(&["fn main() {}"]);
        let value = format!(
            "{}+{}#{}",
            "a".repeat(64),
            "b".repeat(16),
            app.session.package.version
        );
        app.session.package.approval = Some(ApprovedVersion::Fingerprint(value));
        assert!(matches!(
            Baseline::for_session(&app.session),
            Baseline::Loading(version) if version == app.session.package.version
        ));
    }

    #[test]
    fn cancellation_prevents_starting_cargo() {
        let mut command = MetadataCommand::new();
        command.cargo_path("/nonexistent/cargo");
        let error = execute_metadata(&command, &AtomicBool::new(true)).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }
}
