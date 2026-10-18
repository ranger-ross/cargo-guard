use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use eyre::Context;
use serde::{Deserialize, Serialize};

use crate::fingerprint::{ParsedFingerprint, fingerprints_match};

pub mod edit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalScope {
    Workspace,
    Global,
}

impl std::fmt::Display for ApprovalScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Workspace => "workspace",
            Self::Global => "global",
        })
    }
}

#[derive(Clone, Debug)]
pub struct ApprovalTarget {
    pub path: PathBuf,
    pub scope: ApprovalScope,
}

pub struct RunnerPaths {
    pub workspace_root: Option<PathBuf>,
    pub global: Option<PathBuf>,
}

pub struct LoadedApprovals {
    pub target: ApprovalTarget,
    pub config: Config,
}

const GLOBAL_PATH: &str = "CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH";
const WORKSPACE_ROOT: &str = "CARGO_GUARD_WORKSPACE_ROOT";

impl RunnerPaths {
    pub fn from_env() -> eyre::Result<Self> {
        let workspace_root = validate_path(WORKSPACE_ROOT, env::var_os(WORKSPACE_ROOT))?;
        let global = validate_path(GLOBAL_PATH, env::var_os(GLOBAL_PATH))?;
        eyre::ensure!(
            workspace_root.is_some() || global.is_some(),
            "set CARGO_GUARD_WORKSPACE_ROOT or CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH in Cargo [env] configuration"
        );
        Ok(Self {
            workspace_root,
            global,
        })
    }
}

fn validate_path(name: &str, value: Option<std::ffi::OsString>) -> eyre::Result<Option<PathBuf>> {
    value.map(|value| {
        let path = PathBuf::from(value);
        eyre::ensure!(
            !path.as_os_str().is_empty() && path.is_absolute(),
            "{name} must be a nonempty absolute path; use an absolute path or Cargo relative = true"
        );
        Ok(path)
    }).transpose()
}

pub fn review_global_path(cwd: &Path) -> eyre::Result<Option<PathBuf>> {
    let config = cargo_config2::Config::load_with_cwd(cwd)
        .context("could not load Cargo configuration for review")?;
    let exported = env::var_os(GLOBAL_PATH);
    let value = match config.env.get(GLOBAL_PATH) {
        Some(entry) if entry.force || exported.is_none() => Some(entry.value.clone()),
        _ => exported,
    };
    validate_path(GLOBAL_PATH, value)
}

pub fn review_target(
    workspace_root: &Path,
    global: bool,
    configured_global: Option<&Path>,
) -> eyre::Result<ApprovalTarget> {
    if !global {
        return Ok(ApprovalTarget {
            path: config_path(workspace_root),
            scope: ApprovalScope::Workspace,
        });
    }
    let path = if let Some(path) = configured_global {
        path.to_owned()
    } else {
        let home = match env::var_os("CARGO_HOME").filter(|value| !value.is_empty()) {
            Some(home) => std::path::absolute(home).context("could not resolve CARGO_HOME")?,
            None => env::home_dir()
                .ok_or_else(|| {
                    eyre::eyre!(
                        "could not find home directory; set {GLOBAL_PATH} to an absolute path"
                    )
                })?
                .join(".cargo"),
        };
        config_path(&home)
    };
    Ok(ApprovalTarget {
        path,
        scope: ApprovalScope::Global,
    })
}

pub fn load_runner_approvals(paths: &RunnerPaths) -> eyre::Result<LoadedApprovals> {
    let workspace = paths.workspace_root.as_deref().map(|root| ApprovalTarget {
        path: config_path(root),
        scope: ApprovalScope::Workspace,
    });
    if let Some(target) = workspace.as_ref()
        && let Some(config) = read_runner_config(&target.path)?
    {
        return Ok(LoadedApprovals {
            target: target.clone(),
            config,
        });
    }
    let target = match &paths.global {
        Some(path) => ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Global,
        },
        None => workspace.ok_or_else(|| eyre::eyre!("no approval destination configured"))?,
    };
    let config = if target.scope == ApprovalScope::Global {
        read_runner_config(&target.path)?
    } else {
        None
    }
    .unwrap_or_else(|| Config {
        approved: BTreeMap::new(),
    });
    Ok(LoadedApprovals { target, config })
}

fn read_runner_config(path: &Path) -> eyre::Result<Option<Config>> {
    read_optional(path)?
        .map(|contents| {
            toml::from_str(&contents)
                .with_context(|| format!("could not parse config {}", path.display()))
        })
        .transpose()
}

pub(super) fn read_optional(path: &Path) -> eyre::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::symlink_metadata(path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Ok(_) => {
                    Err(error).with_context(|| format!("could not read config {}", path.display()))
                }
                Err(error) => Err(error)
                    .with_context(|| format!("could not inspect config {}", path.display())),
            }
        }
        Err(error) => {
            Err(error).with_context(|| format!("could not read config {}", path.display()))
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub approved: BTreeMap<String, ApprovedVersion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum BlanketApproval {
    #[serde(rename = "approved")]
    Approved,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ApprovedVersion {
    Blanket(BlanketApproval),
    SemVer(semver::Version),
    Fingerprint(String),
    Multiple(#[serde(deserialize_with = "deserialize_approvals")] Vec<ApprovedVersion>),
}

impl ApprovedVersion {
    pub fn is_blanket(&self) -> bool {
        match self {
            Self::Blanket(_) => true,
            Self::Multiple(entries) => entries.iter().any(Self::is_blanket),
            _ => false,
        }
    }

    pub fn entries(&self) -> Vec<&Self> {
        match self {
            Self::Multiple(entries) => entries.iter().flat_map(Self::entries).collect(),
            _ => vec![self],
        }
    }

    pub fn allows_version(&self, version: &semver::Version) -> bool {
        match self {
            Self::Blanket(_) => true,
            Self::SemVer(approved) => approved == version,
            Self::Multiple(entries) => entries.iter().any(|entry| entry.allows_version(version)),
            Self::Fingerprint(_) => false,
        }
    }

    pub fn allows_fingerprint(&self, fingerprint: &str) -> bool {
        match self {
            Self::Blanket(_) => true,
            Self::Fingerprint(approved) => fingerprints_match(approved, fingerprint),
            Self::Multiple(entries) => entries
                .iter()
                .any(|entry| entry.allows_fingerprint(fingerprint)),
            Self::SemVer(_) => false,
        }
    }

    /// The current approval, or the nearest historical version for source review.
    /// The complete allow list remains in ConfigFile when editing.
    pub fn for_review(&self, current: &semver::Version) -> Option<Self> {
        if self.is_blanket() {
            return Some(Self::Blanket(BlanketApproval::Approved));
        }
        let entries = self.entries();
        let mut versions: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                Self::SemVer(v) => Some(v),
                _ => None,
            })
            .collect();
        versions.sort();
        versions
            .iter()
            .rev()
            .find(|v| ***v <= *current)
            .or_else(|| versions.first())
            .map(|v| Self::SemVer((*v).clone()))
            .or_else(|| {
                entries
                    .iter()
                    .filter_map(|entry| match entry {
                        Self::Fingerprint(value) => {
                            ParsedFingerprint::parse(value).map(|parsed| (value, parsed.version))
                        }
                        _ => None,
                    })
                    .min_by(|(_, a), (_, b)| match (a <= current, b <= current) {
                        (true, true) => b.cmp(a),
                        (false, false) => a.cmp(b),
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                    })
                    .map(|(value, _)| Self::Fingerprint(value.clone()))
            })
            .or_else(|| {
                entries.iter().find_map(|entry| match entry {
                    Self::Fingerprint(f) => Some(Self::Fingerprint(f.clone())),
                    _ => None,
                })
            })
    }
}

pub fn config_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join("approved-builds.toml")
}

#[cfg(test)]
pub fn load_config_from(path: &Path) -> eyre::Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("could not read config {}", path.display()))?;

    toml::from_str(&contents).with_context(|| format!("could not parse config {}", path.display()))
}

fn deserialize_approvals<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ApprovedVersion>, D::Error> {
    Vec::<String>::deserialize(deserializer).and_then(|entries| {
        entries
            .into_iter()
            .map(|entry| {
                if entry == "approved" {
                    return Err(serde::de::Error::custom(
                        "\"approved\" must be a standalone string, not a list entry",
                    ));
                }
                Ok(entry
                    .parse()
                    .map(ApprovedVersion::SemVer)
                    .unwrap_or(ApprovedVersion::Fingerprint(entry)))
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_runner_files_are_empty_without_creating_them() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global/approved-builds.toml");
        for paths in [
            RunnerPaths {
                workspace_root: Some(root.path().to_owned()),
                global: None,
            },
            RunnerPaths {
                workspace_root: None,
                global: Some(global.clone()),
            },
            RunnerPaths {
                workspace_root: Some(root.path().to_owned()),
                global: Some(global.clone()),
            },
        ] {
            let loaded = load_runner_approvals(&paths).unwrap();
            assert!(loaded.config.approved.is_empty());
            assert_eq!(
                loaded.target.scope,
                if paths.global.is_some() {
                    ApprovalScope::Global
                } else {
                    ApprovalScope::Workspace
                }
            );
            assert!(!loaded.target.path.exists());
        }
        assert!(!global.parent().unwrap().exists());
    }

    #[test]
    fn explicit_review_destination_does_not_change_workspace_policy() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("personal.toml");
        let workspace = review_target(root.path(), false, Some(&global)).unwrap();
        assert_eq!(workspace.path, config_path(root.path()));
        assert_eq!(workspace.scope, ApprovalScope::Workspace);
        let personal = review_target(root.path(), true, Some(&global)).unwrap();
        assert_eq!(personal.path, global);
        assert_eq!(personal.scope, ApprovalScope::Global);
        for invalid in ["", ".", "relative", "~/personal.toml"] {
            assert!(validate_path(GLOBAL_PATH, Some(invalid.into())).is_err());
        }
        assert_eq!(
            validate_path(GLOBAL_PATH, Some(global.into_os_string())).unwrap(),
            Some(personal.path)
        );
        assert_eq!(validate_path(GLOBAL_PATH, None).unwrap(), None);
    }

    #[test]
    fn blanket_approval_matches_all_versions_and_inputs_but_requires_exact_keyword() {
        let config: Config =
            toml::from_str("[approved]\nall='approved'\ncase='Approved'\nspace='approved '\n")
                .unwrap();
        let roundtrip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(roundtrip.approved, config.approved);
        let approval = &config.approved["all"];
        for version in ["0.0.1", "1.0.0", "99.0.0-rc.1+build.2"] {
            let version = version.parse().unwrap();
            assert!(approval.allows_version(&version));
            assert_eq!(
                approval.for_review(&version),
                Some(ApprovedVersion::Blanket(BlanketApproval::Approved))
            );
        }
        assert!(approval.allows_fingerprint(&format!("{:064x}+{:016x}#3.0.0", 1, 2)));
        for key in ["case", "space"] {
            assert!(!config.approved[key].is_blanket());
            assert!(!config.approved[key].allows_version(&"1.0.0".parse().unwrap()));
        }
    }

    #[test]
    fn blanket_approval_is_rejected_inside_lists() {
        for value in [
            "['approved']",
            "['1.0.0', 'approved']",
            "['approved', '1.0.0']",
            "['old-hash', 'approved', '2.0.0']",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("[approved]\nfoo={value}\n")).is_err(),
                "accepted blanket approval in {value}"
            );
        }
    }

    #[test]
    fn mixed_lists_match_independently_and_select_nearest_baseline() {
        let first = format!("{:064x}+{:016x}#1.0.0", 1, 2);
        let second = format!("{:064x}+{:016x}#2.0.0", 3, 4);
        let config: Config = toml::from_str(&format!(
            "[approved]\npkg=['2.0.0','{first}','1.0.0','{second}']\nempty=[]\n"
        ))
        .unwrap();
        let approvals = &config.approved["pkg"];
        assert!(approvals.allows_version(&"1.0.0".parse().unwrap()));
        assert!(approvals.allows_version(&"2.0.0".parse().unwrap()));
        assert!(!approvals.allows_version(&"3.0.0".parse().unwrap()));
        assert!(approvals.allows_fingerprint(&first));
        assert!(approvals.allows_fingerprint(&second));
        for (current, expected) in [
            ("0.5.0", "1.0.0"),
            ("1.5.0", "1.0.0"),
            ("2.0.0", "2.0.0"),
            ("3.0.0", "2.0.0"),
        ] {
            assert_eq!(
                approvals.for_review(&current.parse().unwrap()),
                Some(ApprovedVersion::SemVer(expected.parse().unwrap()))
            );
        }
        assert!(
            config.approved["empty"]
                .for_review(&"1.0.0".parse().unwrap())
                .is_none()
        );
        let roundtrip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(config.approved, roundtrip.approved);
        for invalid in ["[['1.0.0']]", "[42]", "true"] {
            assert!(toml::from_str::<Config>(&format!("[approved]\npkg={invalid}")).is_err());
        }
    }

    #[test]
    fn fingerprint_hints_select_baselines_without_authorizing_versions() {
        let old = format!("{:064x}+{:016x}#1.0.0", 1, 2);
        let new = format!("{:064x}+{:016x}#2.0.0", 1, 3);
        let approvals = ApprovedVersion::Multiple(vec![
            ApprovedVersion::Fingerprint("legacy-hash".into()),
            ApprovedVersion::Fingerprint(new.clone()),
            ApprovedVersion::Fingerprint(old.clone()),
        ]);
        for (current, expected) in [
            ("0.5.0", &old),
            ("1.5.0", &old),
            ("2.0.0", &new),
            ("3.0.0", &new),
        ] {
            let version = current.parse().unwrap();
            assert_eq!(
                approvals.for_review(&version),
                Some(ApprovedVersion::Fingerprint(expected.clone()))
            );
            assert!(!approvals.allows_version(&version));
        }
        assert!(approvals.allows_fingerprint(&format!("{:064x}+{:016x}#9.0.0", 1, 2)));
        assert!(!approvals.allows_fingerprint(&format!("{:064x}+{:016x}#1.0.0", 2, 2)));
        assert!(!approvals.allows_fingerprint(&format!("{:064x}+{:016x}#1.0.0", 1, 4)));
        assert!(!approvals.allows_fingerprint("legacy-hash"));
    }

    #[test]
    fn detects_versions_and_fingerprints_and_round_trips() {
        let hash = format!("{:064x}+{:016x}#1.0.0-rc.1+build.2", 1, 2);
        let source = format!(
            "[approved]\nfoo = \"1.0.0\"\nbar = \"{hash}\"\npreview = \"2.0.0-rc.1+build.2\"\n"
        );
        let config: Config = toml::from_str(&source).unwrap();
        for config in [
            toml::from_str::<Config>(&toml::to_string(&config).unwrap()).unwrap(),
            config,
        ] {
            assert!(matches!(
                &config.approved["foo"],
                ApprovedVersion::SemVer(version) if version == &semver::Version::new(1, 0, 0)
            ));
            assert!(matches!(
                &config.approved["bar"],
                ApprovedVersion::Fingerprint(value) if value == &hash
            ));
            assert!(matches!(
                &config.approved["preview"],
                ApprovedVersion::SemVer(version) if version.to_string() == "2.0.0-rc.1+build.2"
            ));
        }
    }
}
