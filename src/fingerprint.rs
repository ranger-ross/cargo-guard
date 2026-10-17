use std::{
    cell::RefCell,
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use eyre::{Context, ContextCompat};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct BuildScriptFingerprint {
    pub name: String,
    pub version: String,
    manifest_path: PathBuf,
    dep_info_path: PathBuf,
    fingerprint: RefCell<Option<String>>,
}

impl BuildScriptFingerprint {
    pub fn current(bin: &Path) -> eyre::Result<Self> {
        let manifest_path = PathBuf::from(
            env::var_os("CARGO_MANIFEST_PATH").context("could not read CARGO_MANIFEST_PATH")?,
        );
        Ok(Self {
            name: env::var("CARGO_PKG_NAME").context("could not read CARGO_PKG_NAME")?,
            version: env::var("CARGO_PKG_VERSION").context("could not read CARGO_PKG_VERSION")?,
            manifest_path,
            dep_info_path: bin.with_extension("d"),
            fingerprint: RefCell::new(None),
        })
    }

    pub fn fingerprint(&self) -> eyre::Result<String> {
        if let Some(fingerprint) = self.fingerprint.borrow().as_ref() {
            return Ok(fingerprint.clone());
        }

        let manifest_dir = self
            .manifest_path
            .parent()
            .context("manifest has no parent")?;
        let dep_info = fs::read_to_string(&self.dep_info_path)
            .with_context(|| format!("could not read dep-info {}", self.dep_info_path.display()))?;

        let dep_info_root = env::var_os("CARGO_GUARD_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| manifest_dir.to_path_buf());
        let fingerprint = from_dep_info(&self.manifest_path, &dep_info, &dep_info_root)?;
        *self.fingerprint.borrow_mut() = Some(fingerprint.clone());
        Ok(fingerprint)
    }
}

pub(crate) fn from_dep_info(
    manifest_path: &Path,
    dep_info: &str,
    dep_info_root: &Path,
) -> eyre::Result<String> {
    let manifest = read_manifest(manifest_path)?;
    let manifest_dir = manifest_path.parent().context("manifest has no parent")?;
    let mut dependencies = manifest.clone();
    // Resolve inherited declarations, while retaining declared version
    // requirements rather than claiming to pin lockfile-resolved versions.
    let needs_workspace = dependency_tables(&manifest).iter().any(|(_, table)| {
        table.as_table().is_some_and(|table| {
            table
                .values()
                .any(|dep| dep.get("workspace").and_then(toml::Value::as_bool) == Some(true))
        })
    });
    if needs_workspace {
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(manifest_path)
            .no_deps()
            .exec()
            .context("could not resolve workspace build dependencies")?;
        let workspace = read_manifest(metadata.workspace_root.join("Cargo.toml").as_std_path())?;
        inherit_dependencies(&mut dependencies, &workspace)?;
    }

    fingerprint_inputs(
        &manifest,
        &dependencies,
        manifest_dir,
        dep_info,
        dep_info_root,
    )
}

// rustc emits Make-style rules, including escaped spaces, line continuations,
// repeated prerequisite lists, empty rules, and env-dep comments.
pub(crate) fn parse_dep_info(contents: &str) -> eyre::Result<BTreeSet<PathBuf>> {
    let contents = contents.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut paths = BTreeSet::new();
    let mut saw_rule = false;
    for line in contents.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let mut chars = line.chars().peekable();
        let mut prerequisites = false;
        let mut word = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars
                    .peek()
                    .is_some_and(|c| c.is_whitespace() || matches!(c, '#' | ':' | '\\')) =>
                {
                    word.push(chars.next().unwrap());
                }
                ':' if !prerequisites && chars.peek().is_none_or(|c| c.is_whitespace()) => {
                    prerequisites = true;
                    saw_rule = true;
                    word.clear();
                }
                '#' => break,
                '$' if chars.peek() == Some(&'$') => {
                    chars.next();
                    word.push('$');
                }
                c if c.is_whitespace() => {
                    if prerequisites && !word.is_empty() {
                        paths.insert(PathBuf::from(std::mem::take(&mut word)));
                    } else {
                        word.clear();
                    }
                }
                c => word.push(c),
            }
        }
        eyre::ensure!(prerequisites, "invalid dep-info rule: {line}");
        if !word.is_empty() {
            paths.insert(PathBuf::from(word));
        }
    }
    eyre::ensure!(
        saw_rule && !paths.is_empty(),
        "dep-info contains no tracked files"
    );
    Ok(paths)
}

fn read_manifest(path: &Path) -> eyre::Result<toml::Value> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("could not read manifest {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("could not parse manifest {}", path.display()))
}

fn dependency_tables(manifest: &toml::Value) -> Vec<(String, &toml::Value)> {
    let mut tables = Vec::new();
    if let Some(deps) = manifest.get("build-dependencies") {
        tables.push(("build-dependencies".into(), deps));
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (target, value) in targets {
            if let Some(deps) = value.get("build-dependencies") {
                tables.push((format!("target.{target}.build-dependencies"), deps));
            }
        }
    }
    tables.sort_by(|a, b| a.0.cmp(&b.0));
    tables
}

fn inherit_dependencies(manifest: &mut toml::Value, workspace: &toml::Value) -> eyre::Result<()> {
    fn resolve(table: &mut toml::Value, workspace: &toml::Value) -> eyre::Result<()> {
        if let Some(deps) = table
            .get_mut("build-dependencies")
            .and_then(toml::Value::as_table_mut)
        {
            for (name, dep) in deps {
                if dep.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    let inherited = workspace
                        .get("workspace")
                        .and_then(|w| w.get("dependencies"))
                        .and_then(|d| d.get(name))
                        .with_context(|| format!("missing workspace dependency {name}"))?;
                    // Keep both the inherited declaration and local overrides
                    // (for example, additional features) in the fingerprint.
                    *dep = toml::Value::Array(vec![inherited.clone(), dep.clone()]);
                }
            }
        }
        Ok(())
    }
    resolve(manifest, workspace)?;
    if let Some(targets) = manifest
        .get_mut("target")
        .and_then(toml::Value::as_table_mut)
    {
        for (_, target) in targets.iter_mut() {
            resolve(target, workspace)?;
        }
    }
    Ok(())
}

fn fingerprint_inputs(
    manifest: &toml::Value,
    dependencies: &toml::Value,
    manifest_dir: &Path,
    dep_info: &str,
    dep_info_root: &Path,
) -> eyre::Result<String> {
    let mut files = BTreeSet::new();
    match manifest.get("package").and_then(|p| p.get("build")) {
        None | Some(toml::Value::Boolean(true)) => {
            files.insert(PathBuf::from("build.rs"));
        }
        Some(toml::Value::String(path)) => {
            files.insert(PathBuf::from(path));
        }
        Some(toml::Value::Array(paths)) => {
            // Cargo's dep-info belongs to this binary and includes its entry
            // point. Other scripts in the package are independent approvals.
            eyre::ensure!(!paths.is_empty(), "manifest has no build scripts");
            eyre::ensure!(
                !parse_dep_info(dep_info)?.is_empty(),
                "build script dep-info has no inputs"
            );
        }
        _ => eyre::bail!("manifest does not enable a build script"),
    }
    files.extend(
        parse_dep_info(dep_info)?
            .into_iter()
            .map(|path| dep_info_root.join(path)),
    );

    // Normalize and deduplicate absolute dep-info paths and manifest-relative
    // script paths. Package-local labels do not include the versioned cache path.
    let root = manifest_dir
        .canonicalize()
        .context("could not resolve manifest directory")?;
    let mut inputs = BTreeSet::new();
    for file in files {
        let path = root
            .join(&file)
            .canonicalize()
            .with_context(|| format!("could not resolve tracked file {}", file.display()))?;
        let label = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
        inputs.insert((label, path));
    }

    let mut hash = Sha256::new();
    hash_field(&mut hash, b"cargo-guard-build-fingerprint-v1");
    for (scope, deps) in dependency_tables(dependencies) {
        hash_field(&mut hash, b"dependencies");
        hash_field(&mut hash, scope.as_bytes());
        hash_field(&mut hash, deps.to_string().as_bytes());
    }
    for (label, path) in inputs {
        let content = fs::read(&path)
            .with_context(|| format!("could not read tracked file {}", path.display()))?;
        hash_field(&mut hash, b"file");
        hash_field(
            &mut hash,
            label
                .to_str()
                .context("tracked path is not UTF-8")?
                .as_bytes(),
        );
        hash_field(&mut hash, &content);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(
        manifest: &toml::Value,
        dependencies: &toml::Value,
        manifest_dir: &Path,
        dep_info: &str,
    ) -> eyre::Result<String> {
        fingerprint_inputs(manifest, dependencies, manifest_dir, dep_info, manifest_dir)
    }

    #[test]
    fn multiple_scripts_track_only_their_own_workspace_relative_inputs() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("dep");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("first.rs"), "mod helper; fn main() {}").unwrap();
        fs::write(package.join("second.rs"), "fn main() {}").unwrap();
        fs::write(package.join("helper.rs"), "const VALUE: u8 = 1;").unwrap();
        let manifest: toml::Value =
            toml::from_str("[package]\nbuild=['first.rs','second.rs']\n").unwrap();
        let first = || {
            fingerprint_inputs(
                &manifest,
                &manifest,
                &package,
                "binary: dep/first.rs dep/helper.rs\n",
                root.path(),
            )
            .unwrap()
        };
        let second = || {
            fingerprint_inputs(
                &manifest,
                &manifest,
                &package,
                "binary: dep/second.rs\n",
                root.path(),
            )
            .unwrap()
        };
        let first_hash = first();
        let second_hash = second();
        assert_ne!(first_hash, second_hash);
        fs::write(package.join("helper.rs"), "const VALUE: u8 = 2;").unwrap();
        assert_ne!(first_hash, first());
        assert_eq!(second_hash, second());
    }

    #[test]
    fn parses_rustc_dep_info() {
        let paths = parse_dep_info(concat!(
            "out: build.rs helper\\ file.rs \\\n",
            " included.rs hash\\#file.rs dollar$$file.rs\n",
            "\nout: build.rs\nbuild.rs:\n# env-dep:VALUE=value\n"
        ))
        .unwrap();
        assert_eq!(
            paths,
            [
                "build.rs",
                "helper file.rs",
                "included.rs",
                "hash#file.rs",
                "dollar$file.rs"
            ]
            .map(PathBuf::from)
            .into()
        );
    }

    #[test]
    fn rejects_missing_rules() {
        assert!(parse_dep_info("").is_err());
        assert!(parse_dep_info("not a rule").is_err());
    }

    #[test]
    fn computes_lazily_and_caches_successful_fingerprints() {
        let root = env::temp_dir().join(format!(
            "cargo-guard-lazy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = BuildScriptFingerprint {
            name: "example".into(),
            version: "1.0.0".into(),
            manifest_path: root.join("Cargo.toml"),
            dep_info_path: root.join("build_script_build.d"),
            fingerprint: RefCell::new(None),
        };
        assert!(script.fingerprint().is_err());
        assert!(script.fingerprint.borrow().is_none());

        fs::create_dir_all(&root).unwrap();
        fs::write(&script.manifest_path, "[package]\nname = 'example'\n").unwrap();
        fs::write(&script.dep_info_path, "out: build.rs\n").unwrap();
        fs::write(root.join("build.rs"), "fn main() {}\n").unwrap();
        let hash = script.fingerprint().unwrap();
        assert_eq!(script.fingerprint.borrow().as_ref(), Some(&hash));

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(script.fingerprint().unwrap(), hash);
    }

    #[test]
    fn fingerprints_inputs_without_pinning_package_version() {
        let root = env::temp_dir().join(format!(
            "cargo-guard-fingerprint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("custom.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("helper.rs"), "original").unwrap();
        let mut manifest: toml::Value = toml::from_str(
            r#"
            [package]
            name = "example"
            version = "1.0.0"
            build = "custom.rs"
            [build-dependencies]
            helper = "1.0"
            [target.'cfg(unix)'.build-dependencies]
            platform-helper = { version = "2.0", features = ["extra"] }
        "#,
        )
        .unwrap();
        let dep_info = "out: custom.rs helper.rs\n";
        let initial = fingerprint(&manifest, &manifest, &root, dep_info).unwrap();
        assert_eq!(initial.len(), 64);
        assert_eq!(
            initial,
            fingerprint(
                &manifest,
                &manifest,
                &root,
                "out: helper.rs custom.rs helper.rs\n"
            )
            .unwrap()
        );
        manifest["package"]["version"] = "1.1.0".into();
        assert_eq!(
            initial,
            fingerprint(&manifest, &manifest, &root, dep_info).unwrap()
        );
        fs::write(root.join("helper.rs"), "changed").unwrap();
        assert_ne!(
            initial,
            fingerprint(&manifest, &manifest, &root, dep_info).unwrap()
        );
        fs::write(root.join("helper.rs"), "original").unwrap();
        manifest["target"]["cfg(unix)"]["build-dependencies"]["platform-helper"]["version"] =
            "3.0".into();
        assert_ne!(
            initial,
            fingerprint(&manifest, &manifest, &root, dep_info).unwrap()
        );
        manifest["target"]["cfg(unix)"]["build-dependencies"]["platform-helper"]["version"] =
            "2.0".into();
        manifest["build-dependencies"]["helper"] = "1.1".into();
        assert_ne!(
            initial,
            fingerprint(&manifest, &manifest, &root, dep_info).unwrap()
        );
        fs::remove_file(root.join("helper.rs")).unwrap();
        assert!(fingerprint(&manifest, &manifest, &root, dep_info).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
