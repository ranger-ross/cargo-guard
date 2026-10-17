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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ApprovedVersion {
    SemVer(semver::Version),
    Fingerprint(String),
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

fn determine_workspace_root() -> eyre::Result<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
