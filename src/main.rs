use std::{ffi::OsString, path::Path, process::Command};

use eyre::Context;

use crate::{
    args::{Args, Command as GuardCommand},
    config::{ApprovalScope, LoadedApprovals, RunnerPaths},
};

mod args;
mod config;
mod invocation;
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
    let invocation = invocation::discover()?;
    let policy_workspace = invocation
        .policy_workspace
        .as_ref()
        .unwrap_or(&invocation.workspace);
    let paths = RunnerPaths::from_env(policy_workspace.workspace_root.as_std_path())?;
    let manifest = std::env::var_os("CARGO_MANIFEST_PATH")
        .ok_or_else(|| eyre::eyre!("could not read CARGO_MANIFEST_PATH"))?;
    if !workspace::contains_manifest(policy_workspace, Path::new(&manifest))? {
        check_approval(&config::load_runner_approvals(&paths)?)?;
    }

    let status = Command::new(binary)
        .args(args)
        .env(
            invocation::POLICY_PID,
            invocation
                .policy_owner
                .unwrap_or_else(std::process::id)
                .to_string(),
        )
        .status()
        .with_context(|| format!("could not run build script {}", binary.to_string_lossy()))?;

    std::process::exit(status.code().unwrap_or(1));
}

fn check_approval(approvals: &LoadedApprovals) -> eyre::Result<()> {
    let name = std::env::var("CARGO_PKG_NAME").context("could not read CARGO_PKG_NAME")?;
    let version = std::env::var("CARGO_PKG_VERSION").context("could not read CARGO_PKG_VERSION")?;
    let approval = approvals.config.approved.get(&name);
    let parsed: semver::Version = version.parse()?;
    if approval.is_some_and(|a| a.allows_version(&parsed)) {
        return Ok(());
    }
    let target = &approvals.target;
    let flag = if target.scope == ApprovalScope::Global {
        " --global"
    } else {
        ""
    };
    let guidance = format!(
        "Approvals ({}): {}\nRun cargo guard review{flag} {name}@{version}",
        target.scope,
        target.path.display(),
    );
    match approval {
        Some(approval) => eyre::bail!(
            "build script for {name} v{version} is not approved: no matching version\nExisting approvals: {}\n{guidance}",
            toml::Value::try_from(approval)?
        ),
        None => eyre::bail!(
            "build script for {name} v{version} is not approved: no approval entry\n{guidance}"
        ),
    }
}
