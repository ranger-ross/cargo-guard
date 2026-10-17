use std::{env, process::Command};

use eyre::Context;

use crate::config::{ApprovedVersion, load_config};

mod config;

fn main() -> eyre::Result<()> {
    let build_script = BuildScriptVersion::current()?;

    let config = load_config()?;

    eyre::ensure!(
        matches!(
            config.approved.get(&build_script.name),
            Some(ApprovedVersion::SemVer(version)) if version == &build_script.version
        ),
        "unapproved build script for {} v{}; approve this version in approved-builds.toml",
        build_script.name,
        build_script.version
    );

    let mut args = env::args_os().skip(1);
    let binary = args.next().ok_or_else(|| {
        eyre::eyre!("expected the build script binary path as the first argument")
    })?;
    let status = Command::new(&binary)
        .args(args)
        .status()
        .with_context(|| format!("could not run build script {}", binary.to_string_lossy()))?;

    std::process::exit(status.code().unwrap_or(1));
}

#[derive(Debug)]
struct BuildScriptVersion {
    name: String,
    version: String,
    // TODO: Hash?
}

impl BuildScriptVersion {
    pub fn current() -> eyre::Result<Self> {
        Ok(Self {
            name: env::var("CARGO_PKG_NAME").context("could not read CARGO_PKG_NAME")?,
            version: env::var("CARGO_PKG_VERSION").context("could not read CARGO_PKG_VERSION")?,
        })
    }
}
