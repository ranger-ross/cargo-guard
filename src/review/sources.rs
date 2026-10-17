use std::{
    fs,
    path::PathBuf,
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

use super::ReviewSession;
use crate::config::ApprovedVersion;

#[derive(Debug)]
pub struct PackageSources {
    pub version: semver::Version,
    pub scripts: Vec<ScriptSource>,
}

#[derive(Debug)]
pub struct ScriptSource {
    pub path: PathBuf,
    pub contents: String,
}

#[derive(Debug)]
pub enum Baseline {
    NewPackage,
    CurrentVersion,
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
        match &session.package.approval {
            None => Self::NewPackage,
            Some(ApprovedVersion::Fingerprint(_)) => Self::Fingerprint,
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
            Self::Fingerprint => {
                "Baseline: fingerprint approvals are not supported by version review".into()
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

struct SourceRequest {
    name: String,
    version: semver::Version,
    source: Option<String>,
    workspace_root: PathBuf,
}

/// Fetching is separate from presentation. Providers return plain source text;
/// they never compile the package or execute any of its build scripts.
trait SourceProvider {
    fn load(&self, request: &SourceRequest, cancelled: &AtomicBool)
    -> eyre::Result<PackageSources>;
}

struct CargoSourceProvider;

impl SourceProvider for CargoSourceProvider {
    fn load(
        &self,
        request: &SourceRequest,
        cancelled: &AtomicBool,
    ) -> eyre::Result<PackageSources> {
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
        let package = metadata.packages.iter().find(|package| {
            package.name.as_str() == request.name
                && package.version == request.version
                && package.source.as_ref().map(|source| source.repr.as_str()) == request.source.as_deref()
        }).context("Cargo did not return the exact approved package version from the original registry")?;
        read_package_sources(package)
    }
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
        let request = SourceRequest {
            name: session.package.name.clone(),
            version: version.clone(),
            source: session
                .package
                .source
                .as_ref()
                .map(|source| source.repr.clone()),
            workspace_root: session
                .config
                .path()
                .parent()
                .context("approval file has no parent")?
                .to_path_buf(),
        };
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker = thread::Builder::new()
            .name("approved-sources".into())
            .spawn(move || {
                let result = CargoSourceProvider.load(&request, &worker_cancelled);
                let baseline = match result {
                    Ok(sources) => Baseline::Ready(sources),
                    Err(error) => Baseline::Unavailable {
                        version,
                        reason: format!("{error:#}"),
                    },
                };
                let _ = sender.send(baseline);
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
}

impl Drop for BaselineLoader {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

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
        };
        let error = CargoSourceProvider
            .load(&request, &AtomicBool::new(false))
            .unwrap_err();
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

    #[test]
    fn cancellation_prevents_starting_cargo() {
        let mut command = MetadataCommand::new();
        command.cargo_path("/nonexistent/cargo");
        let error = execute_metadata(&command, &AtomicBool::new(true)).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }
}
