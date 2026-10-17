use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use eyre::{Context, ContextCompat};

use super::ReviewSession;
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
    from_directory(session, root, directory.as_std_path())
        .wrap_err("Could not approve fingerprints. Run cargo build with the desired target/features, then reopen review; an approval failure during that build is expected")
}

fn from_directory(
    session: &ReviewSession,
    root: &Path,
    directory: &Path,
) -> eyre::Result<Vec<ApprovedVersion>> {
    let mut artifacts = Vec::new();
    collect(directory, &mut artifacts)?;
    let mut result = BTreeSet::new();
    for script in &session.package.scripts {
        let script_path = script.path.canonicalize()?;
        let stem = script.name.replace('-', "_");
        let mut candidates = Vec::new();
        for artifact in &artifacts {
            let name = artifact
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if name != stem
                && !name
                    .strip_prefix(&format!("{stem}-"))
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_hexdigit())
                    })
            {
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
            .chain([
                session.package.manifest_path.clone(),
                root.join("Cargo.toml"),
            ])
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
                    session.package.name
                );
            }
            Ok(())
        };
        check_fresh()?;
        result.insert(fingerprint::from_dep_info(
            &session.package.manifest_path,
            contents,
            root,
        )?);
        check_fresh()?;
        eyre::ensure!(
            fs::read_to_string(artifact)? == *contents,
            "compiled inputs changed during review; reopen review"
        );
    }
    eyre::ensure!(!result.is_empty(), "no build scripts to fingerprint");
    Ok(result
        .into_iter()
        .map(ApprovedVersion::Fingerprint)
        .collect())
}

fn collect(directory: &Path, result: &mut Vec<PathBuf>) -> eyre::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
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
            collect(&entry.path(), result)?;
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

    #[test]
    fn approval_uses_runner_hash_and_replaces_scalar_fingerprint() {
        let (root, mut app, artifact) = compiled_fixture();
        let expected = fingerprint::from_dep_info(
            &app.session.package.manifest_path,
            &fs::read_to_string(&artifact).unwrap(),
            root.path(),
        )
        .unwrap();
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
        assert!(from_directory(&app.session, root.path(), &target).is_err());
        fs::write(target.join("script1.d"), "binary: script1.rs\n").unwrap();
        let hashes = from_directory(&app.session, root.path(), &target).unwrap();
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
            from_directory(&app.session, root.path(), &target).unwrap(),
            hashes
        );
    }
}
