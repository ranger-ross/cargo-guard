use std::{fs, io::Write, path::Path};

use eyre::{Context, ContextCompat};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value};

#[cfg(test)]
use super::config_path;
use super::{ApprovalScope, ApprovalTarget, ApprovedVersion, Config, read_optional};

/// A review session's config snapshot. Successful edits are saved immediately;
/// failed saves leave both the snapshot and the in-memory approvals unchanged.
pub struct ConfigFile {
    target: ApprovalTarget,
    original: Option<String>,
    document: DocumentMut,
    config: Config,
}

impl ConfigFile {
    #[cfg(test)]
    pub fn for_workspace(root: &Path) -> eyre::Result<Self> {
        Self::open(ApprovalTarget {
            path: config_path(root),
            scope: ApprovalScope::Workspace,
        })
    }

    /// A missing file is an empty allow list until the first approval is saved.
    pub fn open(mut target: ApprovalTarget) -> eyre::Result<Self> {
        target.path = std::path::absolute(&target.path).context("could not resolve config path")?;
        let original = read_optional(&target.path)?;
        if target.scope == ApprovalScope::Global && original.is_some() {
            target.path = target
                .path
                .canonicalize()
                .with_context(|| format!("could not resolve config {}", target.path.display()))?;
        }
        let path = &target.path;
        let document = original
            .as_deref()
            .unwrap_or("")
            .parse::<DocumentMut>()
            .with_context(|| format!("could not parse config {}", path.display()))?;
        let config = parse_config(&document, path)?;
        Ok(Self {
            target,
            original,
            document,
            config,
        })
    }

    pub fn path(&self) -> &Path {
        &self.target.path
    }

    pub fn target(&self) -> &ApprovalTarget {
        &self.target
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Replaces workspace scalar approvals absent from the selected graph.
    /// Global approvals and existing arrays append. Blanket approvals are unchanged.
    pub fn add_version(
        &mut self,
        package: &str,
        version: &semver::Version,
        graph_versions: &[semver::Version],
    ) -> eyre::Result<()> {
        let replace_scalar = match self.config.approved.get(package) {
            Some(approval) if approval.is_blanket() => return Ok(()),
            Some(ApprovedVersion::SemVer(old)) if old == version => return Ok(()),
            Some(ApprovedVersion::Multiple(old)) if old.contains(version) => return Ok(()),
            Some(ApprovedVersion::SemVer(old)) => {
                self.target.scope == ApprovalScope::Workspace && !graph_versions.contains(old)
            }
            _ => false,
        };
        let mut document = self.document.clone();
        let table = document
            .entry("approved")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .context("approved must be a TOML table")?;
        let new = version.to_string();
        let mut value = if replace_scalar {
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
        let config = parse_config(&document, self.path())?;
        let contents = document.to_string();
        let mut path = self.target.path.clone();
        let mut lock = None;
        if self.target.scope == ApprovalScope::Global {
            let parent = path.parent().context("config path has no parent")?;
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "could not create global approval directory {}",
                    parent.display()
                )
            })?;
            path = parent
                .canonicalize()
                .with_context(|| {
                    format!(
                        "could not resolve global approval directory {}",
                        parent.display()
                    )
                })?
                .join(path.file_name().context("config path has no filename")?);
            let mut name = path.file_name().unwrap().to_os_string();
            name.push(".lock");
            let lock_path = path.with_file_name(name);
            let handle = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .with_context(|| {
                    format!(
                        "could not open global approval lock {}",
                        lock_path.display()
                    )
                })?;
            match handle.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => eyre::bail!(
                    "global approvals {} are locked by another save. Retry shortly",
                    path.display()
                ),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("could not lock global approvals {}", path.display())
                    });
                }
            }
            lock = Some(handle);
        }
        let parent = path.parent().context("config path has no parent")?;
        // Same-directory staging keeps the rename on the same filesystem.
        let mut temporary = NamedTempFile::new_in(parent)
            .with_context(|| format!("could not stage config in {}", parent.display()))?;
        if self.original.is_some() {
            let permissions = fs::metadata(&path)
                .with_context(|| format!("could not read config metadata {}", path.display()))?
                .permissions();
            temporary.as_file().set_permissions(permissions)?;
        }
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().sync_all()?;

        // Non-cooperating editors can still race this comparison and rename.
        eyre::ensure!(
            read_optional(&path)? == self.original,
            "config {} changed on disk. Reload before saving",
            path.display()
        );
        if self.original.is_none() {
            temporary.persist_noclobber(&path)
        } else {
            temporary.persist(&path)
        }
        .with_context(|| format!("could not save config {}", path.display()))?;

        self.target.path = path;
        self.original = Some(contents);
        self.document = document;
        self.config = config;
        drop(lock);
        Ok(())
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

    fn version(value: &str) -> semver::Version {
        value.parse().unwrap()
    }

    #[test]
    fn workspace_replaces_scalar_absent_from_graph_in_either_approval_order() {
        for (old, new) in [("1.0.0", "2.0.0"), ("2.0.0", "1.0.0")] {
            let root = tempfile::tempdir().unwrap();
            let path = config_path(root.path());
            let original = format!(
                "# Workspace approvals\n[approved]\n# Keep this explanation\nexample  = '{old}' # reviewed\nother = '3.0.0'\n"
            );
            fs::write(&path, &original).unwrap();
            let mut file = ConfigFile::for_workspace(root.path()).unwrap();
            let graph_versions = [version(new)];

            file.add_version("example", &version(new), &graph_versions)
                .unwrap();

            assert_eq!(
                file.config.approved["example"],
                ApprovedVersion::SemVer(version(new))
            );
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                original.replace(&format!("'{old}'"), &format!("\"{new}\""))
            );
            assert_eq!(
                super::super::load_config_from(&path).unwrap().approved["example"],
                ApprovedVersion::SemVer(version(new))
            );
        }
    }

    #[test]
    fn workspace_retains_scalar_present_in_graph_in_either_approval_order() {
        let graph_versions = [version("1.0.0"), version("2.0.0")];
        for (first, second) in [("1.0.0", "2.0.0"), ("2.0.0", "1.0.0")] {
            let root = tempfile::tempdir().unwrap();
            let mut file = ConfigFile::for_workspace(root.path()).unwrap();
            file.add_version("example", &version(first), &graph_versions)
                .unwrap();
            assert_eq!(
                file.config.approved["example"],
                ApprovedVersion::SemVer(version(first))
            );

            file.add_version("example", &version(second), &graph_versions)
                .unwrap();

            let expected = ApprovedVersion::Multiple(vec![version(first), version(second)]);
            assert_eq!(file.config.approved["example"], expected);
            assert_eq!(
                super::super::load_config_from(file.path())
                    .unwrap()
                    .approved["example"],
                expected
            );
        }
    }

    #[test]
    fn repeated_scalar_approval_is_a_noop_even_when_absent_from_graph() {
        for scope in [ApprovalScope::Workspace, ApprovalScope::Global] {
            let root = tempfile::tempdir().unwrap();
            let path = config_path(root.path());
            let original = "[approved]\nexample = '1.0.0' # reviewed\n";
            fs::write(&path, original).unwrap();
            let mut file = ConfigFile::open(ApprovalTarget {
                path: path.clone(),
                scope,
            })
            .unwrap();
            let external = format!("# External edit\n{original}");
            fs::write(&path, &external).unwrap();

            file.add_version("example", &version("1.0.0"), &[]).unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), external);
            assert_eq!(file.original.as_deref(), Some(original));
            assert_eq!(
                file.config.approved["example"],
                ApprovedVersion::SemVer(version("1.0.0"))
            );
        }
    }

    #[test]
    fn review_preserves_manual_blanket_approvals_without_creating_them() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        let original = "[approved]\nexample = 'approved' # trusted\n";
        fs::write(&path, original).unwrap();
        let mut file = ConfigFile::for_workspace(root.path()).unwrap();
        file.add_version("example", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
        file.remove_version("example", &version("2.0.0")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        file.add_version("other", &version("1.0.0"), &[version("1.0.0")])
            .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("example = 'approved' # trusted"));
        assert!(file.config.approved["example"].is_blanket());
    }

    #[test]
    fn appends_without_duplicates_and_removes_only_selected_version() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        fs::write(
            &path,
            "[approved]\nexample = [\n  '0.9.0', # first\n  '1.0.0', # old version\n] # package\n",
        )
        .unwrap();
        let mut file = ConfigFile::open(ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Workspace,
        })
        .unwrap();
        file.add_version("example", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
        assert_eq!(
            file.config.approved["example"],
            ApprovedVersion::Multiple(vec![version("0.9.0"), version("1.0.0"), version("2.0.0")])
        );
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# first"));
        assert!(saved.contains("# old version"));
        assert!(saved.contains("# package"));
        file.add_version("example", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        file.remove_version("example", &version("2.0.0")).unwrap();
        assert!(file.config.approved["example"].allows_version(&version("0.9.0")));
        assert!(file.config.approved["example"].allows_version(&version("1.0.0")));
        assert!(!file.config.approved["example"].allows_version(&version("2.0.0")));
    }

    #[test]
    fn creates_config_on_first_approval_and_supports_repeated_edits() {
        let root = tempfile::tempdir().unwrap();
        let mut file = ConfigFile::for_workspace(root.path()).unwrap();
        assert_eq!(file.path(), config_path(root.path()));
        assert!(file.config().approved.is_empty());
        file.remove_approval("absent").unwrap();
        assert!(!file.path().exists());

        let graph_versions = [version("1.0.0"), version("1.1.0")];
        file.add_version("example", &version("1.0.0"), &graph_versions)
            .unwrap();
        file.add_version("another", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
        file.add_version("example", &version("1.1.0"), &graph_versions)
            .unwrap();
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
        let mut file = ConfigFile::open(ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Workspace,
        })
        .unwrap();
        file.add_version(
            "example",
            &version("1.1.0"),
            &[version("1.0.0"), version("1.1.0")],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replace("'1.0.0'", "[\"1.0.0\", \"1.1.0\"]")
        );
        file.add_version("new-package", &version("3.0.0"), &[version("3.0.0")])
            .unwrap();
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
        let mut first = ConfigFile::open(ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Workspace,
        })
        .unwrap();
        let mut second = ConfigFile::open(ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Workspace,
        })
        .unwrap();
        first
            .add_version("example", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(
            second
                .add_version("example", &version("3.0.0"), &[version("3.0.0")])
                .is_err()
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
        assert!(
            file.add_version("example", &version("1.0.0"), &[version("1.0.0")])
                .is_err()
        );
        assert_eq!(fs::read_to_string(file.path()).unwrap(), external);
        assert!(file.config().approved.is_empty());
    }

    #[test]
    fn rejects_invalid_configs_and_supports_inline_approval_tables() {
        let root = tempfile::tempdir().unwrap();
        let path = config_path(root.path());
        for invalid in ["[broken", "approved = 42", "[approved]\nexample = false"] {
            fs::write(&path, invalid).unwrap();
            assert!(
                ConfigFile::open(ApprovalTarget {
                    path: path.clone(),
                    scope: ApprovalScope::Workspace
                })
                .is_err()
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        }
        fs::write(&path, "approved = { example = '1.0.0' } # inline\n").unwrap();
        let mut file = ConfigFile::open(ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Workspace,
        })
        .unwrap();
        file.add_version("example", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
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
        let mut file = ConfigFile::open(ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Workspace,
        })
        .unwrap();
        file.add_version("example", &version("1.0.0"), &[version("1.0.0")])
            .unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("# settings\n[settings]\nvalue = 7\n")
        );
        assert!(file.config().approved.contains_key("example"));
    }

    #[test]
    fn global_saves_append_and_preserve_snapshot_under_contention() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/approvals.toml");
        let target = ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Global,
        };
        let mut first = ConfigFile::open(target.clone()).unwrap();
        assert!(!path.parent().unwrap().exists());
        first
            .add_version("example", &version("1.0.0"), &[version("1.0.0")])
            .unwrap();
        let mut stale = ConfigFile::open(target.clone()).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_file_name("approvals.toml.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(
            first
                .add_version("example", &version("2.0.0"), &[version("2.0.0")])
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(first.original.as_deref(), Some(before.as_str()));
        assert!(!first.config.approved["example"].allows_version(&version("2.0.0")));
        drop(lock);
        first
            .add_version("example", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
        assert!(first.config.approved["example"].allows_version(&version("1.0.0")));
        let saved = fs::read_to_string(&path).unwrap();
        assert!(
            stale
                .add_version("example", &version("3.0.0"), &[version("3.0.0")])
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        let mut reopened = ConfigFile::open(first.target().clone()).unwrap();
        assert_eq!(reopened.target().scope, ApprovalScope::Global);
        reopened
            .add_version("example", &version("3.0.0"), &[version("3.0.0")])
            .unwrap();
        assert_eq!(
            reopened.config.approved["example"],
            ApprovedVersion::Multiple(vec![version("1.0.0"), version("2.0.0"), version("3.0.0")])
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_edits_follow_symlinks_and_reject_dangling_links() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let real = root_path.join("real");
        fs::create_dir(&real).unwrap();
        let alias = root_path.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let path = real.join("approvals.toml");
        fs::write(&path, "# keep\n[approved]\nexample='1.0.0' # reviewed\nother='3.0.0'\n[settings]\nenabled=true\n").unwrap();
        let link = root_path.join("linked.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let mut file = ConfigFile::open(ApprovalTarget {
            path: link.clone(),
            scope: ApprovalScope::Global,
        })
        .unwrap();
        file.add_version("example", &version("2.0.0"), &[version("2.0.0")])
            .unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(file.path(), path);
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# keep") && saved.contains("# reviewed"));
        assert!(saved.contains("other='3.0.0'") && saved.contains("enabled=true"));
        let mut missing = ConfigFile::open(ApprovalTarget {
            path: alias.join("new.toml"),
            scope: ApprovalScope::Global,
        })
        .unwrap();
        missing
            .add_version("example", &version("1.0.0"), &[version("1.0.0")])
            .unwrap();
        assert_eq!(missing.path(), real.join("new.toml"));
        fs::remove_file(&path).unwrap();
        assert!(
            ConfigFile::open(ApprovalTarget {
                path: link,
                scope: ApprovalScope::Global
            })
            .is_err()
        );
    }
}
