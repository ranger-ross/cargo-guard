use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use eyre::{Context, ContextCompat};
use sha2::{Digest, Sha256};

use super::{ReviewPackage, ReviewSession, sources::ScriptSource};
use crate::{config::ApprovedVersion, fingerprint};

/// Cache only artifact locations, never fingerprints or authorization decisions.
pub(super) struct ArtifactIndex {
    directory: PathBuf,
    artifacts: Vec<PathBuf>,
    directories: Vec<(PathBuf, Option<DirectoryStamp>)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verification {
    Verified(Arc<[ScriptSource]>),
    Mismatch,
    Unavailable(String),
}

#[derive(PartialEq, Eq)]
struct DirectoryStamp {
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl DirectoryStamp {
    fn read(path: &Path) -> eyre::Result<Option<Self>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        eyre::ensure!(metadata.is_dir(), "{} is not a directory", path.display());
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Some(Self {
            modified: metadata.modified()?,
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
            #[cfg(not(unix))]
            created: metadata.created().ok(),
        }))
    }
}

impl ArtifactIndex {
    pub(super) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            artifacts: Vec::new(),
            directories: Vec::new(),
        }
    }

    /// Authenticate every current target. Version suffixes are retrieval hints.
    pub(super) fn verify(
        &mut self,
        package: &ReviewPackage,
        approvals: &ApprovedVersion,
        root: &Path,
        cancelled: &AtomicBool,
    ) -> Verification {
        let fingerprints = (|| {
            self.refresh(cancelled)?;
            let fingerprints =
                calculate(package, root, &self.directory, &self.artifacts, cancelled)?;
            eyre::ensure!(
                Self::unchanged(&self.directories, cancelled)?,
                "compiled artifact directories changed during review"
            );
            Ok::<_, eyre::Report>(fingerprints)
        })();
        match fingerprints {
            Ok(fingerprints) => {
                if fingerprints
                    .iter()
                    .any(|(fingerprint, _)| !approvals.allows_fingerprint(fingerprint))
                {
                    return Verification::Mismatch;
                }
                Verification::Verified(fingerprints.into_iter().map(|(_, source)| source).collect())
            }
            Err(error) => Verification::Unavailable(format!("{error:#}")),
        }
    }

    fn unchanged(
        directories: &[(PathBuf, Option<DirectoryStamp>)],
        cancelled: &AtomicBool,
    ) -> eyre::Result<bool> {
        for (path, stamp) in directories {
            eyre::ensure!(
                !cancelled.load(Ordering::Relaxed),
                "fingerprint verification cancelled"
            );
            if DirectoryStamp::read(path)? != *stamp {
                return Ok(false);
            }
        }
        Ok(!directories.is_empty())
    }

    fn refresh(&mut self, cancelled: &AtomicBool) -> eyre::Result<()> {
        if Self::unchanged(&self.directories, cancelled)? {
            return Ok(());
        }
        let mut artifacts = Vec::new();
        let mut directories = Vec::new();
        collect_cancellable(&self.directory, &mut artifacts, &mut directories, cancelled)?;
        eyre::ensure!(
            Self::unchanged(&directories, cancelled)?,
            "compiled artifact directories changed while indexing"
        );
        self.artifacts = artifacts;
        self.directories = directories;
        Ok(())
    }
}

/// Use already-compiled inputs; reviewing must never run a build script.
pub fn approvals(session: &ReviewSession) -> eyre::Result<Vec<ApprovedVersion>> {
    let root = session
        .config
        .path()
        .parent()
        .context("approval file has no parent")?;
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(root)
        .manifest_path(root.join("Cargo.toml"))
        .no_deps()
        .other_options(vec!["--offline".into()])
        .exec()
        .context("could not locate Cargo build artifacts")?;
    let directory = metadata
        .build_directory
        .as_ref()
        .unwrap_or(&metadata.target_directory);
    from_directory(&session.package, root, directory.as_std_path())
}

fn from_directory(
    package: &ReviewPackage,
    root: &Path,
    directory: &Path,
) -> eyre::Result<Vec<ApprovedVersion>> {
    let cancelled = AtomicBool::new(false);
    let mut index = ArtifactIndex::new(directory.to_path_buf());
    index.refresh(&cancelled)?;
    let fingerprints = calculate(package, root, directory, &index.artifacts, &cancelled)?;
    eyre::ensure!(
        ArtifactIndex::unchanged(&index.directories, &cancelled)?,
        "compiled artifact directories changed during review"
    );
    Ok(fingerprints
        .into_iter()
        .map(|(fingerprint, _)| fingerprint)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(ApprovedVersion::Fingerprint)
        .collect())
}

fn calculate(
    package: &ReviewPackage,
    root: &Path,
    directory: &Path,
    artifacts: &[PathBuf],
    cancelled: &AtomicBool,
) -> eyre::Result<Vec<(String, ScriptSource)>> {
    let mut result = Vec::with_capacity(package.scripts.len());
    let version = package.version.to_string();
    for script in &package.scripts {
        eyre::ensure!(
            !cancelled.load(Ordering::Relaxed),
            "fingerprint verification cancelled"
        );
        let script_path = script.path.canonicalize()?;
        let mut candidates = Vec::new();
        for artifact in artifacts {
            eyre::ensure!(
                !cancelled.load(Ordering::Relaxed),
                "fingerprint verification cancelled"
            );
            if !matches_target(artifact, &script.name) {
                continue;
            }
            let contents = fs::read_to_string(artifact)?;
            let Ok(inputs) = fingerprint::parse_dep_info(&contents) else {
                continue;
            };
            if !inputs.iter().any(|path| {
                root.join(path)
                    .canonicalize()
                    .is_ok_and(|p| p == script_path)
            }) {
                continue;
            }
            candidates.push((
                fs::metadata(artifact)?.modified()?,
                artifact,
                contents,
                inputs,
            ));
        }
        // Different target/feature builds can have different input sets. Use the
        // latest compiled instance, not a union of unrelated configurations.
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        let (compiled, artifact, contents, inputs) = candidates
            .last()
            .with_context(|| format!(
                "No compiled dep-info found for {} v{}.\n\
                 Build a workspace target that uses this dependency with the intended features, target and \
                 Cargo build directory, then retry fingerprint approval.\n\n\
                 Script: {}\n\
                 Searched: {}",
                package.name, package.version, script.path.display(), directory.display()
            ))?;
        let paths: Vec<_> = inputs
            .iter()
            .map(|p| root.join(p))
            .chain([package.manifest_path.clone(), root.join("Cargo.toml")])
            .collect();
        let check_fresh = || -> eyre::Result<()> {
            for path in &paths {
                let modified = fs::metadata(path)
                    .with_context(|| format!("could not read tracked input {}", path.display()))?
                    .modified()?;
                eyre::ensure!(
                    modified <= *compiled,
                    "{} is newer than {}; rebuild to refresh tracked inputs (cargo clean -p {} if needed)",
                    path.display(),
                    artifact.display(),
                    package.name
                );
            }
            Ok(())
        };
        check_fresh()?;
        let fingerprint =
            fingerprint::from_dep_info(&package.manifest_path, contents, root, &version)?;
        let source = fs::read_to_string(&script.path)?;
        let mut snapshot_hash = String::with_capacity(64);
        fingerprint::append_hex(&mut snapshot_hash, &Sha256::digest(source.as_bytes()));
        eyre::ensure!(
            fingerprint::ParsedFingerprint::parse(&fingerprint)
                .is_some_and(|parsed| parsed.script_hash == snapshot_hash),
            "current source snapshot does not match the calculated script hash"
        );
        check_fresh()?;
        eyre::ensure!(
            fs::read_to_string(artifact)? == *contents,
            "compiled inputs changed during review; reopen review"
        );
        eyre::ensure!(
            fs::metadata(artifact)?.modified()? == *compiled,
            "compiled artifact changed during review; reopen review"
        );
        eyre::ensure!(
            !cancelled.load(Ordering::Relaxed),
            "fingerprint verification cancelled"
        );
        let path = package
            .manifest_path
            .parent()
            .and_then(|root| script.path.strip_prefix(root).ok())
            .unwrap_or(&script.path)
            .to_path_buf();
        result.push((
            fingerprint,
            ScriptSource {
                path,
                contents: source,
            },
        ));
    }
    eyre::ensure!(!result.is_empty(), "no build scripts to fingerprint");
    Ok(result)
}

pub(super) fn matches_target(artifact: &Path, target: &str) -> bool {
    let stem = target.replace('-', "_");
    let name = artifact
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name == stem
        || name
            .strip_prefix(&format!("{stem}-"))
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_hexdigit())
            })
}

pub(super) fn collect(directory: &Path, result: &mut Vec<PathBuf>) -> eyre::Result<()> {
    let mut index = ArtifactIndex::new(directory.to_path_buf());
    index.refresh(&AtomicBool::new(false))?;
    result.extend(index.artifacts);
    Ok(())
}

fn collect_cancellable(
    directory: &Path,
    result: &mut Vec<PathBuf>,
    directories: &mut Vec<(PathBuf, Option<DirectoryStamp>)>,
    cancelled: &AtomicBool,
) -> eyre::Result<()> {
    eyre::ensure!(
        !cancelled.load(Ordering::Relaxed),
        "fingerprint verification cancelled"
    );
    let stamp = DirectoryStamp::read(directory)?;
    let absent = stamp.is_none();
    directories.push((directory.to_path_buf(), stamp));
    if absent {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        eyre::ensure!(
            !cancelled.load(Ordering::Relaxed),
            "fingerprint verification cancelled"
        );
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            // Avoid traversing large, irrelevant compiler caches.
            if matches!(
                entry.file_name().to_str(),
                Some("incremental" | "deps" | ".fingerprint" | "examples")
            ) {
                continue;
            }
            collect_cancellable(&entry.path(), result, directories, cancelled)?;
        } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "d") {
            result.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::app::{App, Decision, tests::fixture};

    fn compiled_fixture() -> (tempfile::TempDir, App, PathBuf) {
        let (root, app) = fixture(&["fn main() {}\n"]);
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='example'\nversion='0.1.0'\nedition='2024'\nbuild='script0.rs'\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/lib.rs"), "").unwrap();
        let artifact = root
            .path()
            .join("target/debug/build/example/hash/out/script0.d");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        (root, app, artifact)
    }

    fn verify_saved(app: &App, index: &mut ArtifactIndex) -> Verification {
        index.verify(
            &app.session.package,
            &app.session.config.config().approved["example"],
            app.session.config.path().parent().unwrap(),
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn current_authorization_ignores_hints_and_mixed_version_representatives() {
        let (root, mut app, _artifact) = compiled_fixture();
        let mut index = ArtifactIndex::new(root.path().join("target"));
        app.decide(Decision::ApproveFingerprint).unwrap();
        let ApprovedVersion::Fingerprint(saved) = &app.session.config.config().approved["example"]
        else {
            panic!("expected fingerprint approval");
        };
        let hashes = saved.split_once('#').unwrap().0;
        let fingerprint = ApprovedVersion::Fingerprint(format!("{hashes}#9.8.7"));
        app.session
            .config
            .set_approval("example", fingerprint)
            .unwrap();
        app.session
            .config
            .set_approval(
                "example",
                ApprovedVersion::SemVer(semver::Version::new(0, 0, 1)),
            )
            .unwrap();
        app.session.package.approval = app.session.config.config().approved["example"]
            .for_review(&app.session.package.version);
        // Neither path nor Git packages can supply a historical registry
        // baseline; current authorization must finish before that fallback.
        for source in [None, Some("git+https://example.com/example#01234567")] {
            app.session.package.source =
                source.map(|repr| cargo_metadata::Source { repr: repr.into() });
            let Verification::Verified(sources) = verify_saved(&app, &mut index) else {
                panic!("saved hashes must authorize regardless of version hint");
            };
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].path, PathBuf::from("script0.rs"));
            assert_eq!(sources[0].contents, "fn main() {}\n");
        }
    }

    #[test]
    fn helper_changes_and_missing_or_stale_artifacts_do_not_authorize_current() {
        let (root, mut app, artifact) = compiled_fixture();
        let mut index = ArtifactIndex::new(root.path().join("target"));
        let helper = root.path().join("helper.rs");
        fs::write(&helper, "const VALUE: u8 = 1;\n").unwrap();
        let dep_info = "binary: script0.rs helper.rs\n";
        fs::write(&artifact, dep_info).unwrap();
        app.decide(Decision::ApproveFingerprint).unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));

        fs::write(&helper, "const VALUE: u8 = 2;\n").unwrap();
        fs::write(&artifact, dep_info).unwrap();
        assert_eq!(verify_saved(&app, &mut index), Verification::Mismatch);

        // Restoring the approved bytes is not enough without fresh dep-info.
        fs::write(&helper, "const VALUE: u8 = 1;\n").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(past))
            .unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Unavailable(_)
        ));
        fs::remove_file(&artifact).unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Unavailable(_)
        ));
        fs::write(&artifact, dep_info).unwrap();
        fs::remove_file(helper).unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Unavailable(_)
        ));
    }

    #[test]
    fn current_inputs_are_rehashed_even_when_metadata_is_unchanged() {
        let (root, mut app, artifact) = compiled_fixture();
        let helper = root.path().join("helper.rs");
        fs::write(&helper, "const VALUE: u8 = 1;\n").unwrap();
        fs::write(&artifact, "binary: script0.rs helper.rs\n").unwrap();
        app.decide(Decision::ApproveFingerprint).unwrap();
        let mut index = ArtifactIndex::new(root.path().join("target"));
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));

        let modified = fs::metadata(&helper).unwrap().modified().unwrap();
        fs::write(&helper, "const VALUE: u8 = 2;\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&helper)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(verify_saved(&app, &mut index), Verification::Mismatch);

        fs::write(&helper, "const VALUE: u8 = 1;\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&helper)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));

        let script = root.path().join("script0.rs");
        let modified = fs::metadata(&script).unwrap().modified().unwrap();
        fs::write(&script, "fn main(){ }\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&script)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(verify_saved(&app, &mut index), Verification::Mismatch);
    }

    #[test]
    fn artifact_index_discovers_created_removed_and_renamed_artifacts() {
        let (root, mut app, artifact) = compiled_fixture();
        let helper = root.path().join("feature.rs");
        fs::write(&helper, "const FEATURE: bool = true;\n").unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        app.decide(Decision::ApproveFingerprint).unwrap();
        let target = root.path().join("target");
        let mut index = ArtifactIndex::new(target.clone());
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));

        let newer = artifact.parent().unwrap().join("script0-abcd.d");
        let pending = newer.with_extension("pending");
        let write_newer = |path: &Path| {
            fs::write(path, "binary: script0.rs feature.rs\n").unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(SystemTime::now() + std::time::Duration::from_secs(60)),
                )
                .unwrap();
        };
        write_newer(&pending);
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));
        fs::rename(&pending, &newer).unwrap();
        assert_eq!(verify_saved(&app, &mut index), Verification::Mismatch);
        fs::remove_file(&newer).unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));

        let new_directory = target.join("new-target/debug/build/example");
        fs::create_dir_all(&new_directory).unwrap();
        write_newer(&new_directory.join("script0-abcd.d"));
        assert_eq!(verify_saved(&app, &mut index), Verification::Mismatch);
        let renamed = target.join("renamed-target");
        fs::rename(target.join("new-target"), &renamed).unwrap();
        assert_eq!(verify_saved(&app, &mut index), Verification::Mismatch);
        fs::remove_dir_all(renamed).unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));
    }

    #[test]
    fn artifact_index_recovers_from_absent_and_removed_build_directories() {
        let (root, mut app, artifact) = compiled_fixture();
        app.decide(Decision::ApproveFingerprint).unwrap();
        let target = root.path().join("target");
        fs::remove_dir_all(&target).unwrap();
        let mut index = ArtifactIndex::new(target.clone());
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Unavailable(_)
        ));

        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));
        fs::remove_dir_all(&target).unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Unavailable(_)
        ));
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));
    }

    #[test]
    fn artifact_index_rereads_existing_dep_info_and_build_timestamps() {
        let (root, mut app, artifact) = compiled_fixture();
        let helper = root.path().join("feature.rs");
        fs::write(&helper, "const FEATURE: bool = true;\n").unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        let variant = artifact.parent().unwrap().join("script0-abcd.d");
        fs::write(&variant, "binary: script0.rs feature.rs\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&variant)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::now() - std::time::Duration::from_secs(60)),
            )
            .unwrap();
        app.decide(Decision::ApproveFingerprint).unwrap();
        let mut index = ArtifactIndex::new(root.path().join("target"));
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Verified(_)
        ));

        // No directory entry changes: an existing variant becomes the newest build.
        let compiled = SystemTime::now() + std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&variant)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(compiled))
            .unwrap();
        assert_eq!(verify_saved(&app, &mut index), Verification::Mismatch);

        // Even unchanged dep-info timestamps cannot cache its tracked input set.
        fs::write(&variant, "binary: script0.rs missing.rs\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&variant)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(compiled))
            .unwrap();
        assert!(matches!(
            verify_saved(&app, &mut index),
            Verification::Unavailable(_)
        ));
    }

    #[test]
    fn changed_build_dependency_declarations_do_not_authorize_current() {
        let (root, app, artifact) = compiled_fixture();
        let directory = root.path().join("target");
        let approvals = from_directory(&app.session.package, root.path(), &directory).unwrap();
        let saved = ApprovedVersion::Multiple(approvals);
        let manifest = &app.session.package.manifest_path;
        let original = fs::read_to_string(manifest).unwrap();
        fs::write(
            manifest,
            format!("{original}\n[build-dependencies]\nchanged = '2'\n"),
        )
        .unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        assert_eq!(
            ArtifactIndex::new(directory).verify(
                &app.session.package,
                &saved,
                root.path(),
                &AtomicBool::new(false),
            ),
            Verification::Mismatch
        );
    }

    #[test]
    fn every_current_script_and_latest_input_set_must_be_approved() {
        let (root, app) = fixture(&["fn main() {}\n", "fn main() { /* second */ }\n"]);
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nbuild=['script0.rs','script1.rs']\n",
        )
        .unwrap();
        let target = root.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("script0.d"), "binary: script0.rs\n").unwrap();
        fs::write(target.join("script1.d"), "binary: script1.rs\n").unwrap();
        let approvals = from_directory(&app.session.package, root.path(), &target).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut index = ArtifactIndex::new(target.clone());
        let mut verify = |saved: &ApprovedVersion| {
            index.verify(&app.session.package, saved, root.path(), &cancelled)
        };
        assert_eq!(verify(&approvals[0]), Verification::Mismatch);
        let saved = ApprovedVersion::Multiple(approvals);
        let Verification::Verified(sources) = verify(&saved) else {
            panic!("all saved scripts must be verified");
        };
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].contents, "fn main() {}\n");
        assert_eq!(sources[1].contents, "fn main() { /* second */ }\n");

        fs::write(
            root.path().join("feature.rs"),
            "const FEATURE: bool = true;\n",
        )
        .unwrap();
        // Explicit timestamps ensure the new feature configuration wins even
        // on filesystems whose timestamp resolution merges adjacent writes.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(target.join("script0.d"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        fs::write(
            target.join("script0-abcd.d"),
            "binary: script0.rs feature.rs\n",
        )
        .unwrap();
        assert_eq!(verify(&saved), Verification::Mismatch);
        fs::write(
            target.join("script0-abcd.d"),
            "binary: script0.rs missing.rs\n",
        )
        .unwrap();
        assert!(matches!(verify(&saved), Verification::Unavailable(_)));
        cancelled.store(true, Ordering::Relaxed);
        assert!(matches!(verify(&saved), Verification::Unavailable(_)));
    }

    #[test]
    fn approval_uses_runner_hash_and_replaces_scalar_fingerprint() {
        let (root, mut app, artifact) = compiled_fixture();
        let expected = fingerprint::from_dep_info(
            &app.session.package.manifest_path,
            &fs::read_to_string(&artifact).unwrap(),
            root.path(),
            &app.session.package.version.to_string(),
        )
        .unwrap();
        assert_eq!(
            fingerprint::ParsedFingerprint::parse(&expected)
                .unwrap()
                .version,
            app.session.package.version
        );
        app.session
            .config
            .set_approval("example", ApprovedVersion::Fingerprint("old-hash".into()))
            .unwrap();
        app.session.package.approval = Some(ApprovedVersion::Fingerprint("old-hash".into()));
        assert!(app.decide(Decision::ApproveFingerprint).unwrap());
        assert!(app.pin_dialog.is_none());
        assert_eq!(app.decision, Some(Decision::ApproveFingerprint));
        let saved = &app.session.config.config().approved["example"];
        assert!(!saved.allows_fingerprint("old-hash"));
        assert!(matches!(saved, ApprovedVersion::Fingerprint(_)));
        assert!(saved.allows_fingerprint(&expected));
        assert!(!saved.allows_version(&app.session.package.version));
        assert!(!app.can_decide());
    }

    #[test]
    fn saved_fingerprint_is_recognized_after_reopening_review() {
        use super::super::sources::Baseline;
        use crate::config::edit::ConfigFile;

        let (root, mut app, _artifact) = compiled_fixture();
        app.decide(Decision::ApproveFingerprint).unwrap();
        let session = ReviewSession {
            package: app.session.package.clone(),
            config: ConfigFile::open(app.session.config.path()).unwrap(),
        };
        let mut reopened = App::new(session).unwrap();
        assert_eq!(reopened.decision, None);
        let mut index = ArtifactIndex::new(root.path().join("target"));
        let Verification::Verified(sources) = verify_saved(&reopened, &mut index) else {
            panic!("saved fingerprints must verify after reopening");
        };
        reopened
            .set_baseline(Baseline::CurrentFingerprint(sources))
            .unwrap();
        assert!(
            !reopened.can_decide(),
            "saved fingerprints were not recognized"
        );
        assert_eq!(reopened.decision, None);
    }

    #[test]
    fn missing_stale_and_changed_sources_do_not_save() {
        let (root, mut app, artifact) = compiled_fixture();
        fs::remove_file(&artifact).unwrap();
        assert!(!app.decide(Decision::ApproveFingerprint).unwrap());
        assert!(app.pin_dialog.is_some());
        assert!(!app.refresh_sources().unwrap());
        assert!(app.pin_dialog.is_some());
        assert!(!app.session.config.path().exists());
        assert!(app.session.config.config().approved.is_empty());
        assert!(app.session.package.approval.is_none());
        assert_eq!(app.decision, None);
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(past))
            .unwrap();
        assert!(!app.decide(Decision::ApproveFingerprint).unwrap());
        assert!(app.pin_dialog.is_some());
        assert!(!app.session.config.path().exists());
        assert!(app.session.config.config().approved.is_empty());
        assert_eq!(app.decision, None);
        fs::write(
            root.path().join("script0.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        assert!(app.decide(Decision::ApproveFingerprint).is_err());
        assert!(app.pin_dialog.is_none());
        fs::remove_file(root.path().join("script0.rs")).unwrap();
        assert!(app.decide(Decision::ApproveFingerprint).is_err());
        assert!(app.pin_dialog.is_none());
        assert!(!app.session.config.path().exists());
        assert_eq!(app.decision, None);
    }

    #[test]
    fn fingerprint_save_conflicts_keep_external_changes_and_decision() {
        let (_root, mut app, _artifact) = compiled_fixture();
        fs::write(app.session.config.path(), "[approved]\nexternal='1.0.0'\n").unwrap();
        assert!(app.decide(Decision::ApproveFingerprint).is_err());
        assert!(app.pin_dialog.is_none());
        assert_eq!(app.decision, None);
        assert!(app.session.config.config().approved.is_empty());
        assert_eq!(
            fs::read_to_string(app.session.config.path()).unwrap(),
            "[approved]\nexternal='1.0.0'\n"
        );
    }

    #[test]
    fn fingerprint_persistence_errors_do_not_offer_version_pins() {
        let (_root, mut app, _artifact) = compiled_fixture();
        fs::create_dir(app.session.config.path()).unwrap();
        assert!(app.decide(Decision::ApproveFingerprint).is_err());
        assert!(app.pin_dialog.is_none());
        assert_eq!(app.decision, None);
        assert!(app.session.package.approval.is_none());
        assert!(app.session.config.config().approved.is_empty());
        assert!(app.session.config.path().is_dir());
    }

    #[test]
    fn missing_artifacts_require_explicit_pin_before_persisting() {
        let (_root, mut app, artifact) = compiled_fixture();
        fs::remove_file(&artifact).unwrap();
        assert!(!app.decide(Decision::ApproveFingerprint).unwrap());
        assert!(app.pin_dialog.is_some());
        assert!(app.can_decide());
        assert!(!app.session.config.path().exists());
        assert!(app.session.config.config().approved.is_empty());
        assert!(app.session.package.approval.is_none());
        assert_eq!(app.decision, None);

        assert!(app.decide(Decision::PinVersion).unwrap());
        assert!(app.pin_dialog.is_none());
        assert_eq!(app.decision, Some(Decision::PinVersion));
        assert!(app.is_current_version_approved());
        assert!(!app.can_decide());
        let saved = crate::config::load_config_from(app.session.config.path()).unwrap();
        assert_eq!(
            saved.approved["example"],
            ApprovedVersion::SemVer(app.session.package.version.clone())
        );
        assert!(!artifact.exists());
    }

    #[test]
    fn pin_save_conflict_keeps_dialog_and_does_not_approve() {
        let (_root, mut app, artifact) = compiled_fixture();
        fs::remove_file(artifact).unwrap();
        assert!(!app.decide(Decision::ApproveFingerprint).unwrap());
        fs::write(app.session.config.path(), "[approved]\nexternal='1.0.0'\n").unwrap();
        let error = app.decide(Decision::PinVersion).unwrap_err();
        app.report_decision_error(error);
        assert!(app.pin_dialog.is_some());
        assert!(app.notice.is_none());
        assert_eq!(app.decision, None);
        assert!(app.session.package.approval.is_none());
        assert!(app.session.config.config().approved.is_empty());
        assert_eq!(
            fs::read_to_string(app.session.config.path()).unwrap(),
            "[approved]\nexternal='1.0.0'\n"
        );
    }

    #[test]
    fn all_scripts_need_compiled_inputs_and_latest_configuration_is_selected() {
        let (root, app) = fixture(&["fn main() {}\n", "fn main() {}\n"]);
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nbuild=['script0.rs','script1.rs']\n",
        )
        .unwrap();
        let target = root.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("script0.d"), "binary: script0.rs\n").unwrap();
        assert!(from_directory(&app.session.package, root.path(), &target).is_err());
        fs::write(target.join("script1.d"), "binary: script1.rs\n").unwrap();
        let hashes = from_directory(&app.session.package, root.path(), &target).unwrap();
        assert_eq!(hashes.len(), 2);
        // An older feature variant tracks a missing input and must not win.
        let old = target.join("script0-abcd.d");
        fs::write(&old, "binary: script0.rs missing.rs\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(
                    std::time::SystemTime::now() - std::time::Duration::from_secs(60),
                ),
            )
            .unwrap();
        assert_eq!(
            from_directory(&app.session.package, root.path(), &target).unwrap(),
            hashes
        );
    }
}
