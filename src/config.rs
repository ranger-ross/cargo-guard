use std::{collections::BTreeMap, env, fs, path::PathBuf};

use eyre::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub approved: BTreeMap<String, ApprovedVersion>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ApprovedVersion {
    SemVer(String),
}

pub fn load_config() -> eyre::Result<Config> {
    let workspace_root = determine_workspace_root()?;

    let config_path = workspace_root.join("approved-builds.toml");
    let contents = fs::read_to_string(&config_path)
        .with_context(|| format!("could not read config {}", config_path.display()))?;

    toml::from_str(&contents)
        .with_context(|| format!("could not parse config {}", config_path.display()))
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
        "set CARGO_GUARD_WORKSPACE_ROOT in .cargo/config.toml: \
         [env] CARGO_GUARD_WORKSPACE_ROOT = {{ value = \".\", relative = true }}"
    );

    let metadata = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("could not determine cargo workspace root")?;

    Ok(metadata.workspace_root.into_std_path_buf())
}
