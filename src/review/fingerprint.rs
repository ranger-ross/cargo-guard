use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use eyre::{Context, ContextCompat};
use sha2::{Digest, Sha256};

use super::{ReviewPackage, ReviewSession, sources::ScriptSource};
use crate::{config::ApprovedVersion, fingerprint};

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
        .wrap_err("Could not approve fingerprints. Run cargo build with the desired target/features, then reopen review; an approval failure during that build is expected")
}

fn from_directory(
    package: &ReviewPackage,
    root: &Path,
    directory: &Path,
) -> eyre::Result<Vec<ApprovedVersion>> {
    let fingerprints = calculate(package, root, directory, &AtomicBool::new(false))?;
    Ok(fingerprints
        .into_iter()
        .map(|(fingerprint, _)| fingerprint)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(ApprovedVersion::Fingerprint)
        .collect())
}

/// Authenticate every current target against the complete saved approval list.
/// Version suffixes are retrieval hints, not part of hash authorization.
pub(super) fn verify_current(
    package: &ReviewPackage,
    approvals: &ApprovedVersion,
    root: &Path,
    directory: &Path,
    cancelled: &AtomicBool,
) -> eyre::Result<Vec<ScriptSource>> {
    calculate(package, root, directory, cancelled)?
        .into_iter()
        .map(|(fingerprint, source)| {
            eyre::ensure!(
                approvals.allows_fingerprint(&fingerprint),
                "current compiled inputs are not approved for {}",
                source.path.display()
            );
            Ok(source)
        })
        .collect()
}

fn calculate(
    package: &ReviewPackage,
    root: &Path,
    directory: &Path,
    cancelled: &AtomicBool,
) -> eyre::Result<Vec<(String, ScriptSource)>> {
    let mut artifacts = Vec::new();
    collect_cancellable(directory, &mut artifacts, cancelled)?;
    let mut result = Vec::with_capacity(package.scripts.len());
    let version = package.version.to_string();
    for script in &package.scripts {
        eyre::ensure!(
            !cancelled.load(Ordering::Relaxed),
            "fingerprint verification cancelled"
        );
        let script_path = script.path.canonicalize()?;
        let mut candidates = Vec::new();
        for artifact in &artifacts {
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
            .with_context(|| format!("no compiled dep-info found for {}", script.path.display()))?;
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
    collect_cancellable(directory, result, &AtomicBool::new(false))
}

fn collect_cancellable(
    directory: &Path,
    result: &mut Vec<PathBuf>,
    cancelled: &AtomicBool,
) -> eyre::Result<()> {
    eyre::ensure!(
        !cancelled.load(Ordering::Relaxed),
        "fingerprint verification cancelled"
    );
    if !directory.exists() {
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
            collect_cancellable(&entry.path(), result, cancelled)?;
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

    fn loaded_baseline(app: &App) -> super::super::sources::Baseline {
        use super::super::sources::BaselineLoader;
        use std::time::{Duration, Instant};

        let loader = BaselineLoader::start(&app.session).unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(baseline) = loader.poll() {
                return baseline;
            }
            assert!(Instant::now() < deadline, "approval verification timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn current_authorization_ignores_hints_and_mixed_version_representatives() {
        use super::super::sources::Baseline;

        let (_root, mut app, _artifact) = compiled_fixture();
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
            let Baseline::CurrentFingerprint(sources) = loaded_baseline(&app) else {
                panic!("saved hashes must authorize regardless of version hint");
            };
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].path, PathBuf::from("script0.rs"));
            assert_eq!(sources[0].contents, "fn main() {}\n");
        }
    }

    #[test]
    fn helper_changes_and_missing_or_stale_artifacts_do_not_authorize_current() {
        use super::super::sources::Baseline;

        let (root, mut app, artifact) = compiled_fixture();
        let helper = root.path().join("helper.rs");
        fs::write(&helper, "const VALUE: u8 = 1;\n").unwrap();
        let dep_info = "binary: script0.rs helper.rs\n";
        fs::write(&artifact, dep_info).unwrap();
        app.decide(Decision::ApproveFingerprint).unwrap();
        assert!(matches!(
            loaded_baseline(&app),
            Baseline::CurrentFingerprint(_)
        ));

        fs::write(&helper, "const VALUE: u8 = 2;\n").unwrap();
        fs::write(&artifact, dep_info).unwrap();
        assert!(!matches!(
            loaded_baseline(&app),
            Baseline::CurrentFingerprint(_)
        ));

        // Restoring the approved bytes is not enough without fresh dep-info.
        fs::write(&helper, "const VALUE: u8 = 1;\n").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(past))
            .unwrap();
        assert!(!matches!(
            loaded_baseline(&app),
            Baseline::CurrentFingerprint(_)
        ));
        fs::remove_file(&artifact).unwrap();
        assert!(!matches!(
            loaded_baseline(&app),
            Baseline::CurrentFingerprint(_)
        ));
        fs::write(&artifact, dep_info).unwrap();
        fs::remove_file(helper).unwrap();
        assert!(!matches!(
            loaded_baseline(&app),
            Baseline::CurrentFingerprint(_)
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
        assert!(
            verify_current(
                &app.session.package,
                &saved,
                root.path(),
                &directory,
                &AtomicBool::new(false),
            )
            .is_err()
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
        let verify = |saved: &ApprovedVersion| {
            verify_current(
                &app.session.package,
                saved,
                root.path(),
                &target,
                &cancelled,
            )
        };
        assert!(verify(&approvals[0]).is_err());
        let saved = ApprovedVersion::Multiple(approvals);
        let sources = verify(&saved).unwrap();
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
        assert!(verify(&saved).is_err());
        fs::write(
            target.join("script0-abcd.d"),
            "binary: script0.rs missing.rs\n",
        )
        .unwrap();
        assert!(verify(&saved).is_err());
        cancelled.store(true, Ordering::Relaxed);
        assert!(verify(&saved).is_err());
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
        app.decide(Decision::Approve).unwrap();
        assert_eq!(app.decision, Some(Decision::ApproveFingerprint));
        let saved = &app.session.config.config().approved["example"];
        assert!(!saved.allows_fingerprint("old-hash"));
        assert!(matches!(saved, ApprovedVersion::Fingerprint(_)));
        assert!(saved.allows_fingerprint(&expected));
        assert!(!saved.allows_version(&app.session.package.version));
        assert!(!app.can_decide());
        assert_eq!(
            app.approval_status(),
            "Approved current build-script fingerprints"
        );
    }

    #[test]
    fn saved_fingerprint_is_recognized_after_reopening_review() {
        use super::super::sources::BaselineLoader;
        use crate::config::edit::ConfigFile;
        use std::time::{Duration, Instant};

        let (_root, mut app, _artifact) = compiled_fixture();
        app.decide(Decision::ApproveFingerprint).unwrap();
        let session = ReviewSession {
            package: app.session.package.clone(),
            config: ConfigFile::open(app.session.config.path()).unwrap(),
        };
        let mut reopened = App::new(session).unwrap();
        assert_eq!(reopened.decision, None);
        let loader = BaselineLoader::start(&reopened.session).unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(baseline) = loader.poll() {
                reopened.set_baseline(baseline).unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "approval verification timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
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
        assert!(app.decide(Decision::ApproveFingerprint).is_err());
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(past))
            .unwrap();
        assert!(
            app.decide(Decision::ApproveFingerprint)
                .unwrap_err()
                .to_string()
                .contains("Could not approve fingerprints")
        );
        fs::write(
            root.path().join("script0.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        fs::write(&artifact, "binary: script0.rs\n").unwrap();
        assert!(
            app.decide(Decision::ApproveFingerprint)
                .unwrap_err()
                .to_string()
                .contains("sources changed")
        );
        assert!(!app.session.config.path().exists());
        assert_eq!(app.decision, None);
    }

    #[test]
    fn fingerprint_save_conflicts_keep_external_changes_and_decision() {
        let (_root, mut app, _artifact) = compiled_fixture();
        fs::write(app.session.config.path(), "[approved]\nexternal='1.0.0'\n").unwrap();
        assert!(app.decide(Decision::ApproveFingerprint).is_err());
        assert_eq!(app.decision, None);
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
