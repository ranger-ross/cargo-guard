use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use eyre::{Context, ContextCompat};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value};

use super::{ApprovedVersion, Config, config_path};

/// A review session's config snapshot. Successful edits are saved immediately;
/// failed saves leave both the snapshot and the in-memory approvals unchanged.
pub struct ConfigFile {
    path: PathBuf,
    original: Option<String>,
    document: DocumentMut,
    config: Config,
}

impl ConfigFile {
    pub fn for_workspace(root: &Path) -> eyre::Result<Self> {
        Self::open(&config_path(root))
    }

    /// A missing file is an empty allow list until the first approval is saved.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let path = std::path::absolute(path).context("could not resolve config path")?;
        let original = read_optional(&path)?;
        let document = original
            .as_deref()
            .unwrap_or("")
            .parse::<DocumentMut>()
            .with_context(|| format!("could not parse config {}", path.display()))?;
        let config = parse_config(&document, &path)?;
        Ok(Self {
            path,
            original,
            document,
            config,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set_approval(&mut self, package: &str, approval: ApprovedVersion) -> eyre::Result<()> {
        self.add_approvals(package, &[approval])
    }

    /// Persist approvals together. A scalar fingerprint is replaced by a new
    /// fingerprint batch; explicit arrays retain their additive semantics.
    pub fn add_approvals(
        &mut self,
        package: &str,
        approvals: &[ApprovedVersion],
    ) -> eyre::Result<()> {
        let mut document = self.document.clone();
        let table = document
            .entry("approved")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .context("approved must be a TOML table")?;
        let replace_fingerprint = matches!(
            self.config.approved.get(package),
            Some(ApprovedVersion::Fingerprint(_))
        ) && approvals
            .iter()
            .all(|approval| matches!(approval, ApprovedVersion::Fingerprint(_)));
        for (index, approval) in approvals.iter().enumerate() {
            let new = match approval {
                ApprovedVersion::SemVer(version) => version.to_string(),
                ApprovedVersion::Fingerprint(fingerprint) => fingerprint.clone(),
                ApprovedVersion::Multiple(_) => eyre::bail!("add one approval at a time"),
            };
            let mut value = if replace_fingerprint && index == 0 {
                Value::from(new)
            } else if let Some(old) = table.get(package).and_then(Item::as_value) {
                let mut array = match old {
                    Value::Array(array) => array.clone(),
                    _ => {
                        let mut array = toml_edit::Array::new();
                        array.push(old.as_str().context("approval must be a string or array")?);
                        array
                    }
                };
                if array.iter().any(|v| v.as_str() == Some(&new)) {
                    eyre::ensure!(
                        read_optional(&self.path)? == self.original,
                        "config {} changed since it was loaded; reload it before saving approvals",
                        self.path.display()
                    );
                    continue;
                }
                array.push(new);
                Value::Array(array)
            } else {
                Value::from(new)
            };
            // Replacing a value should retain its spacing and trailing comment.
            if let Some(item) = table.get_mut(package) {
                if let Some(old) = item.as_value() {
                    *value.decor_mut() = old.decor().clone();
                }
                // Keep the existing key too: leading comments belong to its decor.
                *item = Item::Value(value);
            } else {
                table.insert(package, Item::Value(value));
            }
        }
        self.save(document)
    }

    pub fn remove_approval(&mut self, package: &str) -> eyre::Result<()> {
        if !self.config.approved.contains_key(package) {
            return Ok(());
        }
        let mut document = self.document.clone();
        document["approved"]
            .as_table_like_mut()
            .context("approved must be a TOML table")?
            .remove(package);
        self.save(document)
    }

    pub fn remove_version(&mut self, package: &str, version: &semver::Version) -> eyre::Result<()> {
        let mut document = self.document.clone();
        let Some(value) = document
            .get_mut("approved")
            .and_then(|table| table.get_mut(package))
            .and_then(Item::as_value_mut)
        else {
            return Ok(());
        };
        if let Value::Array(array) = value {
            let target = version.to_string();
            array.retain(|entry| entry.as_str() != Some(&target));
            if !array.is_empty() {
                return self.save(document);
            }
        } else if value.as_str() != Some(&version.to_string()) {
            return Ok(());
        }
        self.remove_approval(package)
    }

    fn save(&mut self, document: DocumentMut) -> eyre::Result<()> {
        let config = parse_config(&document, &self.path)?;
        let contents = document.to_string();
        let parent = self.path.parent().context("config path has no parent")?;
        // Same-directory staging keeps the rename on the same filesystem.
        let mut temporary = NamedTempFile::new_in(parent)
            .with_context(|| format!("could not stage config in {}", parent.display()))?;
        if self.original.is_some() {
            let permissions = fs::metadata(&self.path)
                .with_context(|| format!("could not read config metadata {}", self.path.display()))?
                .permissions();
            temporary.as_file().set_permissions(permissions)?;
        }
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().sync_all()?;

        // Optimistic conflict detection: compare immediately before publishing.
        // An external writer can still race the check/rename; this is not a lock.
        eyre::ensure!(
            read_optional(&self.path)? == self.original,
            "config {} changed since it was loaded; reload it before saving approvals",
            self.path.display()
        );
        if self.original.is_none() {
            // Do not overwrite a file created after the comparison above.
            temporary.persist_noclobber(&self.path)
        } else {
            temporary.persist(&self.path)
        }
        .with_context(|| format!("could not save config {}", self.path.display()))?;

        self.original = Some(contents);
        self.document = document;
        self.config = config;
        Ok(())
    }
}

fn read_optional(path: &Path) -> eyre::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("could not read config {}", path.display()))
        }
    }
}

fn parse_config(document: &DocumentMut, path: &Path) -> eyre::Result<Config> {
    let mut document = document.clone();
    // Review may start with an empty or unrelated config; only add the actual
    // table to the saved document when an approval is written.
    document
        .entry("approved")
        .or_insert(Item::Table(Table::new()));
    toml::from_str(&document.to_string())
        .with_context(|| format!("could not parse config {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> ApprovedVersion {
        ApprovedVersion::SemVer(value.parse().unwrap())
    }

    #[test]
    fn scalar_fingerprints_replace_but_explicit_arrays_append() {
        for (original, hashes, expected) in [
            ("'old-hash'", vec!["new-hash"], "\"new-hash\""),
            (
                "['old-hash']",
                vec!["new-hash"],
                "['old-hash', \"new-hash\"]",
            ),
            (
                "'old-hash'",
                vec!["first", "second"],
                "[\"first\", \"second\"]",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = config_path(root.path());
            fs::write(
                &path,
                format!("[approved]\n# explanation\nexample = {original} # retained\n"),
            )
            .unwrap();
            let mut file = ConfigFile::open(&path).unwrap();
            let approvals: Vec<_> = hashes
                .into_iter()
                .map(|hash| ApprovedVersion::Fingerprint(hash.into()))
                .collect();
            file.add_approvals("example", &approvals).unwrap();
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                format!("[approved]\n# explanation\nexample = {expected} # retained\n")
            );
        }
    }

    #[test]
    fn appends_without_duplicates_and_removes_only_selected_version() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        fs::write(&path, "[approved]\nexample = [\n  'fingerprint', # script one\n  '1.0.0', # old version\n] # package\n").unwrap();
        let mut file = ConfigFile::open(&path).unwrap();
        file.set_approval("example", version("2.0.0")).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# script one"));
        assert!(saved.contains("# old version"));
        assert!(saved.contains("# package"));
        file.set_approval("example", version("2.0.0")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        file.remove_version("example", &"2.0.0".parse().unwrap())
            .unwrap();
        assert!(file.config.approved["example"].allows_version(&"1.0.0".parse().unwrap()));
        assert!(file.config.approved["example"].allows_fingerprint("fingerprint"));
        assert!(!file.config.approved["example"].allows_version(&"2.0.0".parse().unwrap()));
    }

    #[test]
    fn fingerprint_batches_are_saved_together_and_conflicts_save_nothing() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        fs::write(&path, "[approved]\nexample='1.0.0' # retained\n").unwrap();
        let mut file = ConfigFile::open(&path).unwrap();
        let batch = [
            ApprovedVersion::Fingerprint("first".into()),
            ApprovedVersion::Fingerprint("second".into()),
        ];
        file.add_approvals("example", &batch).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# retained"));
        assert!(file.config.approved["example"].allows_fingerprint("first"));
        assert!(file.config.approved["example"].allows_fingerprint("second"));
        assert!(file.config.approved["example"].allows_version(&"1.0.0".parse().unwrap()));
        let before = file.config.approved.clone();
        fs::write(&path, "[approved]\nexternal='2.0.0'\n").unwrap();
        assert!(
            file.add_approvals(
                "example",
                &[
                    ApprovedVersion::Fingerprint("third".into()),
                    ApprovedVersion::Fingerprint("fourth".into())
                ]
            )
            .is_err()
        );
        assert_eq!(file.config.approved, before);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[approved]\nexternal='2.0.0'\n"
        );
    }

    #[test]
    fn creates_config_on_first_approval_and_supports_repeated_edits() {
        let root = tempfile::tempdir().unwrap();
        let mut file = ConfigFile::for_workspace(root.path()).unwrap();
        assert_eq!(file.path(), config_path(root.path()));
        assert!(file.config().approved.is_empty());
        file.remove_approval("absent").unwrap();
        assert!(!file.path().exists());

        file.set_approval("example", version("1.0.0")).unwrap();
        file.set_approval("another", version("2.0.0")).unwrap();
        file.set_approval("example", version("1.1.0")).unwrap();
        let loaded = super::super::load_config_from(file.path()).unwrap();
        assert!(loaded.approved["example"].allows_version(&"1.0.0".parse().unwrap()));
        assert!(loaded.approved["example"].allows_version(&"1.1.0".parse().unwrap()));
        file.remove_approval("example").unwrap();
        assert!(!file.config().approved.contains_key("example"));
        file.remove_approval("another").unwrap();
        assert!(
            super::super::load_config_from(file.path())
                .unwrap()
                .approved
                .is_empty()
        );
    }

    #[test]
    fn preserves_comments_formatting_and_unrelated_entries() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        let original = concat!(
            "# Workspace approvals\n",
            "[approved] # reviewed packages\n",
            "# Keep this explanation\n",
            "example = '1.0.0' # approved by reviewer\n",
            "other = '2.0.0' # unchanged\n\n",
            "[settings]\n",
            "enabled = true # unrelated\n"
        );
        fs::write(&path, original).unwrap();
        let mut file = ConfigFile::open(&path).unwrap();
        file.set_approval("example", version("1.1.0")).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replace("'1.0.0'", "[\"1.0.0\", \"1.1.0\"]")
        );
        file.set_approval("new-package", version("3.0.0")).unwrap();
        file.remove_approval("example").unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("other = '2.0.0' # unchanged"));
        assert!(saved.contains("[settings]\nenabled = true # unrelated"));
        assert!(saved.contains("[approved] # reviewed packages"));
    }

    #[test]
    fn detects_external_edits_without_overwriting_or_mutating_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        fs::write(&path, "[approved]\nexample = '1.0.0'\n").unwrap();
        let mut first = ConfigFile::open(&path).unwrap();
        let mut second = ConfigFile::open(&path).unwrap();
        first.set_approval("example", version("2.0.0")).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(
            second
                .set_approval("example", version("3.0.0"))
                .unwrap_err()
                .to_string()
                .contains("changed since it was loaded")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        assert!(
            matches!(&second.config().approved["example"], ApprovedVersion::SemVer(v) if v.to_string() == "1.0.0")
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn detects_creation_after_loading_a_missing_file() {
        let root = tempfile::tempdir().unwrap();
        let mut file = ConfigFile::for_workspace(root.path()).unwrap();
        let external = "# Created elsewhere\n[approved]\n";
        fs::write(file.path(), external).unwrap();
        assert!(file.set_approval("example", version("1.0.0")).is_err());
        assert_eq!(fs::read_to_string(file.path()).unwrap(), external);
        assert!(file.config().approved.is_empty());
    }

    #[test]
    fn rejects_invalid_configs_and_supports_inline_approval_tables() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        for invalid in ["[broken", "approved = 42", "[approved]\nexample = false"] {
            fs::write(&path, invalid).unwrap();
            assert!(ConfigFile::open(&path).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        }
        fs::write(&path, "approved = { example = '1.0.0' } # inline\n").unwrap();
        let mut file = ConfigFile::open(&path).unwrap();
        file.set_approval("example", version("2.0.0")).unwrap();
        file.remove_approval("example").unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("# inline"));
        assert!(
            super::super::load_config_from(&path)
                .unwrap()
                .approved
                .is_empty()
        );
    }

    #[test]
    fn adds_approval_table_to_existing_unrelated_config() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        fs::write(&path, "# settings\n[settings]\nvalue = 7\n").unwrap();
        let mut file = ConfigFile::open(&path).unwrap();
        file.set_approval("example", version("1.0.0")).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("# settings\n[settings]\nvalue = 7\n")
        );
        assert!(file.config().approved.contains_key("example"));
    }
}
