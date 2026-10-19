use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use eyre::Context;
use serde::{Deserialize, Serialize};

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

pub struct RunnerPaths<'a> {
    pub workspace_root: &'a Path,
    pub global: Option<PathBuf>,
}

pub struct LoadedApprovals {
    pub target: ApprovalTarget,
    pub config: Config,
}

const GLOBAL_PATH: &str = "CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH";

impl<'a> RunnerPaths<'a> {
    pub fn from_env(workspace_root: &'a Path) -> eyre::Result<Self> {
        let global = validate_path(GLOBAL_PATH, env::var_os(GLOBAL_PATH))?;
        Ok(Self {
            workspace_root,
            global,
        })
    }
}

fn validate_path(name: &str, value: Option<std::ffi::OsString>) -> eyre::Result<Option<PathBuf>> {
    value
        .map(|value| {
            let path = PathBuf::from(value);
            eyre::ensure!(
                !path.as_os_str().is_empty() && path.is_absolute(),
                "{name} must be an absolute path (or use Cargo relative = true)"
            );
            Ok(path)
        })
        .transpose()
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
                    eyre::eyre!("home directory not found. Set {GLOBAL_PATH} to an absolute path")
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

pub fn load_runner_approvals(paths: &RunnerPaths<'_>) -> eyre::Result<LoadedApprovals> {
    let workspace = ApprovalTarget {
        path: config_path(paths.workspace_root),
        scope: ApprovalScope::Workspace,
    };
    if let Some(config) = read_runner_config(&workspace.path)? {
        return Ok(LoadedApprovals {
            target: workspace,
            config,
        });
    }
    let target = match &paths.global {
        Some(path) => ApprovalTarget {
            path: path.clone(),
            scope: ApprovalScope::Global,
        },
        None => workspace,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum BlanketApproval {
    #[serde(rename = "approved")]
    Approved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ApprovedVersion {
    Blanket(BlanketApproval),
    SemVer(semver::Version),
    Multiple(Vec<semver::Version>),
}

impl ApprovedVersion {
    pub fn is_blanket(&self) -> bool {
        matches!(self, Self::Blanket(_))
    }

    pub fn allows_version(&self, version: &semver::Version) -> bool {
        match self {
            Self::Blanket(_) => true,
            Self::SemVer(approved) => approved == version,
            Self::Multiple(approved) => approved.contains(version),
        }
    }

    /// The current approval, or the nearest historical version for source review.
    /// The complete allow list remains in ConfigFile when editing.
    pub fn for_review(&self, current: &semver::Version) -> Option<Self> {
        let versions = match self {
            Self::Blanket(_) => return Some(self.clone()),
            Self::SemVer(version) => return Some(Self::SemVer(version.clone())),
            Self::Multiple(versions) => versions,
        };
        versions
            .iter()
            .filter(|version| *version <= current)
            .max()
            .or_else(|| versions.iter().min())
            .map(|version| Self::SemVer(version.clone()))
    }
}

impl<'de> Deserialize<'de> for ApprovedVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged, expecting = "a version, a list of versions, or \"approved\"")]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }

        match Raw::deserialize(deserializer)? {
            Raw::One(value) if value == "approved" => Ok(Self::Blanket(BlanketApproval::Approved)),
            Raw::One(value) => parse_version(&value).map(Self::SemVer),
            Raw::Many(values) => values
                .iter()
                .map(|value| {
                    if value == "approved" {
                        return Err(serde::de::Error::custom(
                            "\"approved\" must be a standalone string, not a list entry",
                        ));
                    }
                    parse_version(value)
                })
                .collect::<Result<_, _>>()
                .map(Self::Multiple),
        }
    }
}

fn parse_version<E: serde::de::Error>(value: &str) -> Result<semver::Version, E> {
    value.parse().map_err(|_| {
        E::custom(format!(
            "invalid approval `{value}`: expected a version, a list of versions, or \"approved\""
        ))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_runner_files_are_empty_without_creating_them() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global/approved-builds.toml");
        for paths in [
            RunnerPaths {
                workspace_root: root.path(),
                global: None,
            },
            RunnerPaths {
                workspace_root: root.path(),
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
    fn blanket_approval_matches_all_versions_but_requires_exact_keyword() {
        let config: Config = toml::from_str("[approved]\nall='approved'\n").unwrap();
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
        for value in ["'Approved'", "'approved '"] {
            assert!(toml::from_str::<Config>(&format!("[approved]\nfoo={value}\n")).is_err());
        }
    }

    #[test]
    fn blanket_approval_is_rejected_inside_lists() {
        for value in [
            "['approved']",
            "['1.0.0', 'approved']",
            "['approved', '1.0.0']",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("[approved]\nfoo={value}\n")).is_err(),
                "accepted blanket approval in {value}"
            );
        }
    }

    #[test]
    fn version_lists_match_independently_and_select_nearest_baseline() {
        let config: Config =
            toml::from_str("[approved]\npkg=['2.0.0','1.0.0']\nempty=[]\n").unwrap();
        let approvals = &config.approved["pkg"];
        assert!(approvals.allows_version(&"1.0.0".parse().unwrap()));
        assert!(approvals.allows_version(&"2.0.0".parse().unwrap()));
        assert!(!approvals.allows_version(&"3.0.0".parse().unwrap()));
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
    }

    #[test]
    fn rejects_values_that_are_not_versions() {
        for invalid in [
            "'legacy-hash'",
            "['1.0.0', 'legacy-hash']",
            "[['1.0.0']]",
            "[42]",
            "true",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("[approved]\npkg={invalid}")).is_err(),
                "accepted {invalid}"
            );
        }
        let error = toml::from_str::<Config>("[approved]\npkg='legacy-hash'")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid approval `legacy-hash`"), "{error}");
    }

    #[test]
    fn detects_versions_and_round_trips() {
        let source = "[approved]\nfoo = \"1.0.0\"\npreview = \"2.0.0-rc.1+build.2\"\n";
        let config: Config = toml::from_str(source).unwrap();
        for config in [
            toml::from_str::<Config>(&toml::to_string(&config).unwrap()).unwrap(),
            config,
        ] {
            assert!(matches!(
                &config.approved["foo"],
                ApprovedVersion::SemVer(version) if version == &semver::Version::new(1, 0, 0)
            ));
            assert!(matches!(
                &config.approved["preview"],
                ApprovedVersion::SemVer(version) if version.to_string() == "2.0.0-rc.1+build.2"
            ));
        }
    }
}
