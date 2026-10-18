use std::{
    cell::RefCell,
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use eyre::{Context, ContextCompat};
use sha2::{Digest, Sha256};

const INPUT_HASH_BYTES: usize = 8;

/// The version is only a lookup hint; parsing does not authenticate either hash
/// or establish that sources from this version are a verified review baseline.
#[derive(Debug)]
pub(crate) struct ParsedFingerprint<'a> {
    pub script_hash: &'a str,
    pub inputs_hash: &'a str,
    pub version: semver::Version,
}

impl<'a> ParsedFingerprint<'a> {
    pub(crate) fn parse(fingerprint: &'a str) -> Option<Self> {
        let (hashes, version) = fingerprint.split_once('#')?;
        let (script_hash, inputs_hash) = hashes.split_once('+')?;
        let is_hash = |hash: &str, length: usize| {
            hash.len() == length
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if !is_hash(script_hash, 64) || !is_hash(inputs_hash, INPUT_HASH_BYTES * 2) {
            return None;
        }
        Some(Self {
            script_hash,
            inputs_hash,
            version: semver::Version::parse(version).ok()?,
        })
    }
}

/// Compare both hashes, never the unauthenticated version hint.
pub(crate) fn fingerprints_match(left: &str, right: &str) -> bool {
    match (
        ParsedFingerprint::parse(left),
        ParsedFingerprint::parse(right),
    ) {
        (Some(left), Some(right)) => {
            left.script_hash == right.script_hash && left.inputs_hash == right.inputs_hash
        }
        _ => false,
    }
}

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

    pub fn fingerprint(&self, dep_info_root: &Path) -> eyre::Result<String> {
        if let Some(fingerprint) = self.fingerprint.borrow().as_ref() {
            return Ok(fingerprint.clone());
        }

        let dep_info = fs::read_to_string(&self.dep_info_path)
            .with_context(|| format!("could not read dep-info {}", self.dep_info_path.display()))?;

        let fingerprint = from_dep_info(
            &self.manifest_path,
            &dep_info,
            Some(dep_info_root),
            &self.version,
        )?;
        *self.fingerprint.borrow_mut() = Some(fingerprint.clone());
        Ok(fingerprint)
    }
}

pub(crate) fn from_dep_info(
    manifest_path: &Path,
    dep_info: &str,
    dep_info_root: Option<&Path>,
    version: &str,
) -> eyre::Result<String> {
    semver::Version::parse(version).context("invalid package version")?;
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
        version,
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
    dep_info_root: Option<&Path>,
    version: &str,
) -> eyre::Result<String> {
    let scripts = match manifest.get("package").and_then(|p| p.get("build")) {
        None | Some(toml::Value::Boolean(true)) => vec![Path::new("build.rs")],
        Some(toml::Value::String(path)) => vec![Path::new(path)],
        Some(toml::Value::Array(paths)) => {
            eyre::ensure!(!paths.is_empty(), "manifest has no build scripts");
            paths
                .iter()
                .map(|path| {
                    path.as_str()
                        .map(Path::new)
                        .context("build script path is not a string")
                })
                .collect::<eyre::Result<Vec<_>>>()?
        }
        _ => eyre::bail!("manifest does not enable a build script"),
    };

    // Normalize and deduplicate absolute dep-info paths. Package-local labels
    // do not include the versioned cache path.
    let root = manifest_dir
        .canonicalize()
        .context("could not resolve manifest directory")?;
    let mut inputs = BTreeSet::new();
    for file in parse_dep_info(dep_info)? {
        let path = match dep_info_root {
            Some(root) => root.join(&file).canonicalize(),
            None => {
                eyre::ensure!(
                    file.is_absolute(),
                    "cannot resolve relative build-script input {} without its compiler working directory",
                    file.display()
                );
                file.canonicalize()
            }
        }
            .with_context(|| format!("could not resolve tracked file {}", file.display()))?;
        inputs.insert(path);
    }
    // Each dep-info describes one binary. Never include another declared
    // script unless rustc actually tracked it as an input of this binary.
    let mut entries = BTreeSet::new();
    for script in scripts {
        let path = root.join(script);
        match path.canonicalize() {
            Ok(path) if inputs.contains(&path) => {
                entries.insert(path);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not resolve build script {}", path.display()));
            }
        }
    }
    eyre::ensure!(
        entries.len() == 1,
        "dep-info must contain exactly one declared build script entry point"
    );
    let entry = entries.pop_first().unwrap();
    let entry_content = fs::read(&entry)
        .with_context(|| format!("could not read build script {}", entry.display()))?;
    let script_hash = Sha256::digest(&entry_content);

    let mut hash = Sha256::new();
    hash_field(&mut hash, b"cargo-guard-build-fingerprint-v2");
    hash_field(&mut hash, b"entry");
    hash_field(&mut hash, input_label(&root, &entry)?.as_bytes());
    for (scope, deps) in dependency_tables(dependencies) {
        hash_field(&mut hash, b"dependencies");
        hash_field(&mut hash, scope.as_bytes());
        hash_field(&mut hash, deps.to_string().as_bytes());
    }
    // Sort by the canonical label, not by the absolute installation path.
    let inputs = inputs
        .iter()
        .filter(|path| **path != entry)
        .map(|path| Ok((input_label(&root, path)?, path)))
        .collect::<eyre::Result<BTreeSet<_>>>()?;
    for (label, path) in inputs {
        let content = fs::read(path)
            .with_context(|| format!("could not read tracked file {}", path.display()))?;
        hash_field(&mut hash, b"file");
        hash_field(&mut hash, label.as_bytes());
        hash_field(&mut hash, &content);
    }
    let mut fingerprint = String::with_capacity(66 + INPUT_HASH_BYTES * 2 + version.len());
    append_hex(&mut fingerprint, &script_hash);
    fingerprint.push('+');
    append_hex(&mut fingerprint, &hash.finalize()[..INPUT_HASH_BYTES]);
    fingerprint.push('#');
    fingerprint.push_str(version);
    Ok(fingerprint)
}

fn input_label<'a>(root: &Path, path: &'a Path) -> eyre::Result<&'a str> {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_str()
        .context("tracked path is not UTF-8")
}

pub(crate) fn append_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
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
        fingerprint_inputs(
            manifest,
            dependencies,
            manifest_dir,
            dep_info,
            Some(manifest_dir),
            "1.0.0",
        )
    }

    fn assert_only_inputs_changed(before: &str, after: &str) {
        let before = ParsedFingerprint::parse(before).unwrap();
        let after = ParsedFingerprint::parse(after).unwrap();
        assert_eq!(before.script_hash, after.script_hash);
        assert_ne!(before.inputs_hash, after.inputs_hash);
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
                Some(root.path()),
                "1.0.0",
            )
            .unwrap()
        };
        let second = || {
            fingerprint_inputs(
                &manifest,
                &manifest,
                &package,
                "binary: dep/second.rs\n",
                Some(root.path()),
                "1.0.0",
            )
            .unwrap()
        };
        let first_hash = first();
        let second_hash = second();
        assert_ne!(first_hash, second_hash);
        fs::write(package.join("helper.rs"), "const VALUE: u8 = 2;").unwrap();
        assert_only_inputs_changed(&first_hash, &first());
        assert_eq!(second_hash, second());
        let before_entry_change = first();
        fs::write(
            package.join("first.rs"),
            "mod helper; fn main() { helper(); }",
        )
        .unwrap();
        let after_entry_change = first();
        let before = ParsedFingerprint::parse(&before_entry_change).unwrap();
        let after = ParsedFingerprint::parse(&after_entry_change).unwrap();
        assert_ne!(before.script_hash, after.script_hash);
        assert_eq!(before.inputs_hash, after.inputs_hash);
        assert_eq!(second_hash, second());
        fs::remove_file(package.join("first.rs")).unwrap();
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
        assert!(script.fingerprint(&root).is_err());
        assert!(script.fingerprint.borrow().is_none());

        fs::create_dir_all(&root).unwrap();
        fs::write(&script.manifest_path, "[package]\nname = 'example'\n").unwrap();
        fs::write(
            &script.dep_info_path,
            format!("out: {}\n", root.join("build.rs").display()),
        )
        .unwrap();
        fs::write(root.join("build.rs"), "fn main() {}\n").unwrap();
        let hash = script.fingerprint(&root).unwrap();
        assert_eq!(script.fingerprint.borrow().as_ref(), Some(&hash));

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(script.fingerprint(&root).unwrap(), hash);
    }

    #[test]
    fn separates_entry_bytes_from_helpers_and_build_dependencies() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("custom.rs"), b"abc").unwrap();
        fs::write(root.path().join("helper.rs"), "original").unwrap();
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
        let initial = fingerprint(&manifest, &manifest, root.path(), dep_info).unwrap();
        assert_eq!(
            ParsedFingerprint::parse(&initial).unwrap().script_hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            initial,
            fingerprint(
                &manifest,
                &manifest,
                root.path(),
                "out: helper.rs custom.rs helper.rs\n"
            )
            .unwrap()
        );
        manifest["package"]["version"] = "1.1.0".into();
        let new_version = fingerprint_inputs(
            &manifest,
            &manifest,
            root.path(),
            dep_info,
            Some(root.path()),
            "1.1.0",
        )
        .unwrap();
        assert_ne!(initial, new_version);
        assert!(fingerprints_match(&initial, &new_version));

        fs::write(root.path().join("custom.rs"), b"abc\n").unwrap();
        let entry_changed = fingerprint(&manifest, &manifest, root.path(), dep_info).unwrap();
        let before = ParsedFingerprint::parse(&initial).unwrap();
        let after = ParsedFingerprint::parse(&entry_changed).unwrap();
        assert_ne!(before.script_hash, after.script_hash);
        assert_eq!(before.inputs_hash, after.inputs_hash);
        fs::write(root.path().join("custom.rs"), b"abc").unwrap();

        fs::write(root.path().join("helper.rs"), "changed").unwrap();
        assert_only_inputs_changed(
            &initial,
            &fingerprint(&manifest, &manifest, root.path(), dep_info).unwrap(),
        );
        fs::write(root.path().join("helper.rs"), "original").unwrap();
        manifest["target"]["cfg(unix)"]["build-dependencies"]["platform-helper"]["version"] =
            "3.0".into();
        assert_only_inputs_changed(
            &initial,
            &fingerprint(&manifest, &manifest, root.path(), dep_info).unwrap(),
        );
        manifest["target"]["cfg(unix)"]["build-dependencies"]["platform-helper"]["version"] =
            "2.0".into();
        manifest["build-dependencies"]["helper"] = "1.1".into();
        assert_only_inputs_changed(
            &initial,
            &fingerprint(&manifest, &manifest, root.path(), dep_info).unwrap(),
        );
        fs::remove_file(root.path().join("helper.rs")).unwrap();
        assert!(fingerprint(&manifest, &manifest, root.path(), dep_info).is_err());
    }

    #[test]
    fn parses_strict_fingerprints_and_ignores_only_the_version() {
        let script = "a".repeat(64);
        let inputs = "b".repeat(16);
        let initial = format!("{script}+{inputs}#1.2.3-alpha.1+build.42");
        let parsed = ParsedFingerprint::parse(&initial).unwrap();
        assert_eq!(parsed.script_hash, script);
        assert_eq!(parsed.inputs_hash, inputs);
        assert_eq!(parsed.version.to_string(), "1.2.3-alpha.1+build.42");
        assert!(fingerprints_match(
            &initial,
            &format!("{script}+{inputs}#9.0.0")
        ));
        assert!(!fingerprints_match(
            &initial,
            &format!("{}+{inputs}#1.2.3-alpha.1+build.42", "c".repeat(64))
        ));
        assert!(!fingerprints_match(
            &initial,
            &format!("{script}+{}#1.2.3-alpha.1+build.42", "c".repeat(16))
        ));
        for malformed in [
            String::new(),
            script.clone(),
            format!("{script}#1.0.0"),
            format!("{script}+{inputs}"),
            format!("{script}-{inputs}#1.0.0"),
            format!("{script}-{}#v1.0.0", "b".repeat(64)),
            format!("{script}+{inputs}#"),
            format!("{script}+{inputs}#1.0"),
            format!("{script}+{inputs}#01.0.0"),
            format!("{script}+{inputs}#v1.0.0"),
            format!("{script}+{inputs}#1.0.0#2.0.0"),
            format!("{script}+{inputs}#1.0.0\n"),
            format!("{}+{inputs}#1.0.0", "a".repeat(63)),
            format!("{script}+{}#1.0.0", "b".repeat(15)),
            format!("{script}+{}#1.0.0", "b".repeat(17)),
            format!("{script}+{}#1.0.0", "b".repeat(64)),
            format!("{}+{inputs}#1.0.0", "A".repeat(64)),
            format!("{script}+{}#1.0.0", "B".repeat(16)),
            format!("{}+{inputs}#1.0.0", "g".repeat(64)),
            format!("{script}+{}#1.0.0", "é".repeat(8)),
            format!("{script}+{inputs}+extra#1.0.0"),
        ] {
            assert!(
                ParsedFingerprint::parse(&malformed).is_none(),
                "{malformed}"
            );
            assert!(!fingerprints_match(&malformed, &malformed));
            assert!(!fingerprints_match(&initial, &malformed));
            assert!(!fingerprints_match(&malformed, &initial));
        }
    }

    #[test]
    fn uses_explicit_version_for_inherited_package_versions() {
        let root = tempfile::tempdir().unwrap();
        let manifest = root.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nversion.workspace = true\n").unwrap();
        fs::write(root.path().join("build.rs"), "fn main() {}\n").unwrap();
        let fingerprint = from_dep_info(
            &manifest,
            "out: build.rs\n",
            Some(root.path()),
            "1.2.3+release",
        )
        .unwrap();
        assert_eq!(
            ParsedFingerprint::parse(&fingerprint).unwrap().version,
            semver::Version::parse("1.2.3+release").unwrap()
        );
        for invalid in ["", "1", "1.2", "v1.2.3", "01.2.3", "1.2.3\n"] {
            assert!(
                from_dep_info(&manifest, "out: build.rs\n", Some(root.path()), invalid).is_err()
            );
        }
    }

    #[test]
    fn rootless_fingerprints_require_absolute_inputs() {
        let root = tempfile::tempdir().unwrap();
        let manifest = root.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname='example'\nversion='1.0.0'\n").unwrap();
        fs::write(root.path().join("build.rs"), "fn main() {}").unwrap();
        fs::write(root.path().join("helper.rs"), "const X: u8 = 1;").unwrap();
        let absolute = format!(
            "out: {} {}\n",
            root.path().join("build.rs").display(),
            root.path().join("helper.rs").display()
        );
        let expected = from_dep_info(&manifest, &absolute, Some(root.path()), "1.0.0").unwrap();
        assert_eq!(
            from_dep_info(&manifest, &absolute, None, "1.0.0").unwrap(),
            expected
        );
        for inputs in [
            "out: build.rs helper.rs\n".to_owned(),
            format!(
                "out: {} helper.rs\n",
                root.path().join("build.rs").display()
            ),
        ] {
            assert!(from_dep_info(&manifest, &inputs, None, "1.0.0").is_err());
            assert_eq!(
                from_dep_info(&manifest, &inputs, Some(root.path()), "1.0.0").unwrap(),
                expected
            );
        }
    }

    #[test]
    fn requires_a_unique_declared_entry_point() {
        let root = tempfile::tempdir().unwrap();
        for script in ["first.rs", "second.rs", "helper.rs"] {
            fs::write(root.path().join(script), "fn main() {}").unwrap();
        }
        let manifest: toml::Value =
            toml::from_str("[package]\nbuild = ['first.rs', 'second.rs']").unwrap();
        for dep_info in ["out: helper.rs\n", "out: first.rs second.rs\n"] {
            assert!(fingerprint(&manifest, &manifest, root.path(), dep_info).is_err());
        }
        for declaration in [
            "false",
            "[]",
            "['first.rs', 1]",
            "'missing.rs'",
            "'first.rs'",
        ] {
            let manifest: toml::Value =
                toml::from_str(&format!("[package]\nbuild = {declaration}")).unwrap();
            assert!(
                fingerprint(&manifest, &manifest, root.path(), "out: helper.rs\n").is_err(),
                "{declaration}"
            );
        }
    }

    #[test]
    fn preserves_relocatability_and_canonical_path_deduplication() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let manifest: toml::Value = toml::from_str("[package]\nbuild = true").unwrap();
        for root in [left.path(), right.path()] {
            fs::create_dir(root.join("sub")).unwrap();
            fs::write(root.join("build.rs"), "fn main() {}").unwrap();
            fs::write(root.join("helper.rs"), "helper").unwrap();
        }
        let initial = fingerprint(
            &manifest,
            &manifest,
            left.path(),
            "out: build.rs helper.rs\n",
        )
        .unwrap();
        assert_eq!(
            initial,
            fingerprint(
                &manifest,
                &manifest,
                right.path(),
                "out: sub/../build.rs helper.rs ./helper.rs\n"
            )
            .unwrap()
        );
        let absolute = format!(
            "out: {} {} build.rs\n",
            left.path().join("build.rs").display(),
            left.path().join("helper.rs").display()
        );
        assert_eq!(
            initial,
            fingerprint(&manifest, &manifest, left.path(), &absolute).unwrap()
        );
    }

    #[test]
    fn entry_and_helper_paths_are_part_of_the_opaque_hash() {
        let root = tempfile::tempdir().unwrap();
        for script in ["first.rs", "second.rs"] {
            fs::write(root.path().join(script), "fn main() {}").unwrap();
        }
        for helper in ["helper.rs", "renamed.rs"] {
            fs::write(root.path().join(helper), "helper").unwrap();
        }
        let mut manifest: toml::Value = toml::from_str("[package]\nbuild = 'first.rs'").unwrap();
        let first = fingerprint(
            &manifest,
            &manifest,
            root.path(),
            "out: first.rs helper.rs\n",
        )
        .unwrap();
        let renamed_helper = fingerprint(
            &manifest,
            &manifest,
            root.path(),
            "out: first.rs renamed.rs\n",
        )
        .unwrap();
        assert_only_inputs_changed(&first, &renamed_helper);
        manifest["package"]["build"] = "second.rs".into();
        let second = fingerprint(
            &manifest,
            &manifest,
            root.path(),
            "out: second.rs helper.rs\n",
        )
        .unwrap();
        assert_only_inputs_changed(&first, &second);
    }

    #[test]
    fn inherited_dependencies_retain_workspace_declarations_and_local_overrides() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("build.rs"), "fn main() {}").unwrap();
        let manifest: toml::Value = toml::from_str(
            "[build-dependencies]\nhelper = { workspace = true, features = ['local'] }\n\
             [target.'cfg(unix)'.build-dependencies]\nplatform = { workspace = true }\n",
        )
        .unwrap();
        let mut workspace: toml::Value = toml::from_str(
            "[workspace.dependencies]\nhelper = { version = '1', features = ['shared'] }\n\
             platform = '2'\n",
        )
        .unwrap();
        let mut resolved = manifest.clone();
        inherit_dependencies(&mut resolved, &workspace).unwrap();
        let initial = fingerprint(&manifest, &resolved, root.path(), "out: build.rs\n").unwrap();
        workspace["workspace"]["dependencies"]["helper"]["version"] = "3".into();
        let mut changed = manifest.clone();
        inherit_dependencies(&mut changed, &workspace).unwrap();
        assert_only_inputs_changed(
            &initial,
            &fingerprint(&manifest, &changed, root.path(), "out: build.rs\n").unwrap(),
        );
        workspace["workspace"]["dependencies"]["helper"]["version"] = "1".into();
        workspace["workspace"]["dependencies"]["platform"] = "3".into();
        let mut changed = manifest.clone();
        inherit_dependencies(&mut changed, &workspace).unwrap();
        assert_only_inputs_changed(
            &initial,
            &fingerprint(&manifest, &changed, root.path(), "out: build.rs\n").unwrap(),
        );
        workspace["workspace"]["dependencies"]["platform"] = "2".into();
        let mut changed = manifest.clone();
        changed["build-dependencies"]["helper"]["features"] =
            toml::Value::Array(vec!["different".into()]);
        inherit_dependencies(&mut changed, &workspace).unwrap();
        assert_only_inputs_changed(
            &initial,
            &fingerprint(&manifest, &changed, root.path(), "out: build.rs\n").unwrap(),
        );
        let missing: toml::Value = toml::from_str("[workspace.dependencies]").unwrap();
        assert!(inherit_dependencies(&mut manifest.clone(), &missing).is_err());
    }
}
