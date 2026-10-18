use std::{ffi::OsString, path::Path, process::Command};

use eyre::Context;

use crate::{
    args::{Args, Command as GuardCommand},
    config::{ApprovalScope, ApprovedVersion, LoadedApprovals, RunnerPaths},
    fingerprint::BuildScriptFingerprint,
};

mod args;
mod config;
mod fingerprint;
mod review;
mod workspace;

fn main() {
    let args = Args::parse_args();
    let is_runner = matches!(&args.command, GuardCommand::BuildScript(_));
    let result = match args.command {
        GuardCommand::Review { package, options } => review::run(package.as_deref(), &options),
        GuardCommand::BuildScript(args) => run(&args),
    };
    if let Err(error) = result {
        if is_runner {
            for line in format!("{error:#}").lines() {
                println!("cargo::error= {line}");
            }
            println!("cargo::error= ");
        } else {
            eprintln!("error: {error:#}");
        }
        std::process::exit(1);
    }
}

fn run(args: &[OsString]) -> eyre::Result<()> {
    let (binary, args) = args.split_first().ok_or_else(|| {
        eyre::eyre!("expected the build script binary path as the first argument")
    })?;
    let paths = RunnerPaths::from_env()?;
    let is_member = if let Some(root) = &paths.workspace_root {
        let manifest = std::env::var_os("CARGO_MANIFEST_PATH")
            .ok_or_else(|| eyre::eyre!("could not read CARGO_MANIFEST_PATH"))?;
        workspace::contains_manifest(root, Path::new(&manifest))?
    } else {
        false
    };
    if !is_member {
        check_approval(Path::new(binary), &config::load_runner_approvals(&paths)?)?;
    }

    let status = Command::new(binary)
        .args(args)
        .status()
        .with_context(|| format!("could not run build script {}", binary.to_string_lossy()))?;

    std::process::exit(status.code().unwrap_or(1));
}

fn check_approval(binary: &Path, approvals: &LoadedApprovals) -> eyre::Result<()> {
    let build_script = BuildScriptFingerprint::current(binary)?;
    let target = &approvals.target;
    let flag = if target.scope == ApprovalScope::Global {
        " --global"
    } else {
        ""
    };
    let guidance = || {
        format!(
            "Approvals ({}): {}\nRun cargo guard review{} {}@{} to review this script. If missing, this creates {}.",
            target.scope,
            target.path.display(),
            flag,
            build_script.name,
            build_script.version,
            target.path.display()
        )
    };
    let approval = approvals.config.approved.get(&build_script.name);
    let version: semver::Version = build_script.version.parse()?;
    if approval.is_some_and(|a| a.allows_version(&version)) {
        return Ok(());
    }
    if approval.is_some_and(|a| {
        a.entries()
            .iter()
            .any(|e| matches!(e, ApprovedVersion::Fingerprint(_)))
    }) {
        let fingerprint = build_script.fingerprint()?;
        if approval.is_some_and(|a| a.allows_fingerprint(&fingerprint)) {
            return Ok(());
        }
        let guidance = guidance();
        eyre::bail!(
            "build script for {} v{} is not approved: fingerprint mismatch\nCurrent fingerprint: {}\nExisting approvals: {}\n{guidance}",
            build_script.name,
            build_script.version,
            fingerprint,
            toml::Value::try_from(approval.unwrap())?
        );
    }
    let guidance = guidance();
    match approval {
        Some(approval) => eyre::bail!(
            "build script for {} v{} is not approved: no matching version\nExisting approvals: {}\n{guidance}",
            build_script.name,
            build_script.version,
            toml::Value::try_from(approval)?
        ),
        None => eyre::bail!(
            "build script for {0} v{1} is not approved: no approval entry\n{guidance}",
            build_script.name,
            build_script.version,
        ),
    }
}
