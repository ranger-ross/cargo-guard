use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use eyre::Context;
use serde::{Deserialize, Serialize};

pub mod edit;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub approved: BTreeMap<String, ApprovedVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ApprovedVersion {
    SemVer(semver::Version),
    Fingerprint(String),
    Multiple(#[serde(deserialize_with = "deserialize_approvals")] Vec<ApprovedVersion>),
}

impl ApprovedVersion {
    pub fn entries(&self) -> Vec<&Self> {
        match self {
            Self::Multiple(entries) => entries.iter().flat_map(Self::entries).collect(),
            _ => vec![self],
        }
    }

    pub fn allows_version(&self, version: &semver::Version) -> bool {
        self.entries()
            .iter()
            .any(|entry| matches!(entry, Self::SemVer(v) if v == version))
    }

    pub fn allows_fingerprint(&self, fingerprint: &str) -> bool {
        self.entries()
            .iter()
            .any(|entry| matches!(entry, Self::Fingerprint(f) if f == fingerprint))
    }

    /// The current approval, or the nearest historical version for source review.
    /// The complete allow list remains in ConfigFile when editing.
    pub fn for_review(&self, current: &semver::Version) -> Option<Self> {
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
                entries.iter().find_map(|entry| match entry {
                    Self::Fingerprint(f) => Some(Self::Fingerprint(f.clone())),
                    _ => None,
                })
            })
    }
}

pub fn load_config() -> eyre::Result<Config> {
    let workspace_root = determine_workspace_root()?;
    load_config_from(&config_path(&workspace_root))
}

pub fn config_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join("approved-builds.toml")
}

pub fn load_config_from(path: &Path) -> eyre::Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("could not read config {}", path.display()))?;

    toml::from_str(&contents).with_context(|| format!("could not parse config {}", path.display()))
}

pub fn determine_workspace_root() -> eyre::Result<PathBuf> {
    // A build-script runner's working directory belongs to the dependency,
    // not the workspace that is building it. Cargo config must supply the root.
    if let Some(root) = env::var_os("CARGO_GUARD_WORKSPACE_ROOT") {
        let root = PathBuf::from(root);
        eyre::ensure!(
            root.is_absolute(),
            "CARGO_GUARD_WORKSPACE_ROOT must be an absolute path; use relative = true in Cargo config"
        );
        return Ok(root);
    }

    // If we are running as a build script runner, we cannot trust `cargo metadata` since it will
    // return the workspace root of the registry package, not the actual workspace being built.
    eyre::ensure!(
        env::var_os("OUT_DIR").is_none(),
        "set CARGO_GUARD_WORKSPACE_ROOT in .cargo/config.toml:\n\
         ```\n\
         [env]\n\
         CARGO_GUARD_WORKSPACE_ROOT = {{ value = \".\", relative = true }}\n\
         ```"
    );

    let metadata = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("could not determine cargo workspace root")?;

    Ok(metadata.workspace_root.into_std_path_buf())
}

fn deserialize_approvals<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ApprovedVersion>, D::Error> {
    Vec::<String>::deserialize(deserializer).map(|entries| {
        entries
            .into_iter()
            .map(|entry| {
                entry
                    .parse()
                    .map(ApprovedVersion::SemVer)
                    .unwrap_or(ApprovedVersion::Fingerprint(entry))
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_lists_match_independently_and_select_nearest_baseline() {
        let config: Config =
            toml::from_str("[approved]\npkg=['2.0.0','hash-a','1.0.0','hash-b']\nempty=[]\n")
                .unwrap();
        let approvals = &config.approved["pkg"];
        assert!(approvals.allows_version(&"1.0.0".parse().unwrap()));
        assert!(approvals.allows_version(&"2.0.0".parse().unwrap()));
        assert!(!approvals.allows_version(&"3.0.0".parse().unwrap()));
        assert!(approvals.allows_fingerprint("hash-a"));
        assert!(approvals.allows_fingerprint("hash-b"));
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
    fn detects_versions_and_fingerprints_and_round_trips() {
        let hash = "7c22171135c52f3285ec525c724cf784149707b935293dd6d158eff15b7bd3e8";
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
                ApprovedVersion::Fingerprint(value) if value == hash
            ));
            assert!(matches!(
                &config.approved["preview"],
                ApprovedVersion::SemVer(version) if version.to_string() == "2.0.0-rc.1+build.2"
            ));
        }
    }
}
