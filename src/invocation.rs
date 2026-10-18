use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
};

use cargo_metadata::{Metadata, MetadataCommand};
use eyre::{Context, ensure};
use serde::Deserialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const MAX_ANCESTORS: usize = 128;
pub const POLICY_PID: &str = "CARGO_GUARD_POLICY_PID";

pub struct Invocation {
    pub workspace: Metadata,
    pub policy_workspace: Option<Metadata>,
    pub policy_owner: Option<u32>,
}

pub fn discover() -> eyre::Result<Invocation> {
    discover_inner().wrap_err("could not discover the invoking Cargo workspace")
}

fn discover_inner() -> eyre::Result<Invocation> {
    let cargo =
        std::env::var_os("CARGO").ok_or_else(|| eyre::eyre!("Cargo did not provide CARGO"))?;
    let cargo = canonical_executable(Path::new(&cargo))?;
    let guard = canonical_executable(&std::env::current_exe()?)?;
    let current_pid = Pid::from_u32(std::process::id());
    // Guard passes its identity to nested builds, never a trusted path.
    let policy_owner = std::env::var_os(POLICY_PID)
        .map(|value| parse_policy_owner(&value, current_pid))
        .transpose()?;
    let mut system = System::new();
    let current = snapshot(&mut system, current_pid)?;
    let mut next = current.parent;
    let mut ancestors = Vec::new();
    let mut child_start = current.start;
    let mut selection = CargoSelection::default();
    while let Some(pid) = next {
        if is_root(pid) {
            break;
        }
        ensure!(
            ancestors.len() < MAX_ANCESTORS,
            "process ancestry exceeds {MAX_ANCESTORS} entries"
        );
        ensure!(
            pid != current_pid && !ancestors.iter().any(|p: &ProcessSnapshot| p.pid == pid),
            "process ancestry contains a cycle"
        );
        let process = snapshot(&mut system, pid)?;
        ensure!(
            process.start <= child_start,
            "ancestor process {pid} was replaced during discovery"
        );
        child_start = process.start;
        next = process.parent;
        selection.observe(
            process.role(&cargo, &guard)?,
            ancestors.len(),
            policy_owner == Some(pid.as_u32()),
        )?;
        ancestors.push(process);
        // Ordinary builds need no ancestry beyond Cargo. Nested builds stop at the owner's Cargo.
        if selection.complete(policy_owner.is_some()) {
            break;
        }
    }

    let (nearest, policy) = selection.finish(policy_owner.is_some())?;
    let workspace = metadata(&ancestors[nearest], &cargo)?;
    let policy_workspace = policy
        .map(|index| metadata(&ancestors[index], &cargo))
        .transpose()?;

    // A fresh system avoids retaining fields when a second process read fails.
    let mut refreshed = System::new();
    for previous in std::iter::once(&current).chain(&ancestors) {
        let now = snapshot(&mut refreshed, previous.pid)?;
        ensure!(
            now == *previous,
            "process {} changed during workspace discovery",
            previous.pid
        );
    }
    Ok(Invocation {
        workspace,
        policy_workspace,
        policy_owner,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ProcessSnapshot {
    pid: Pid,
    parent: Option<Pid>,
    start: u64,
    exe: PathBuf,
    cmd: Vec<OsString>,
    cwd: Option<PathBuf>,
    environ: Vec<OsString>,
}

impl ProcessSnapshot {
    fn role(&self, cargo: &Path, guard: &Path) -> eyre::Result<Role> {
        if self.exe == cargo || is_cargo_name(&self.exe) {
            ensure!(
                self.cmd.first().is_some_and(|arg| !arg.is_empty()),
                "Cargo process {} has inaccessible arguments",
                self.pid
            );
            return Ok(Role::Cargo);
        }
        if self.exe == guard {
            ensure!(
                self.cmd.first().is_some_and(|arg| !arg.is_empty()),
                "cargo-guard process {} has inaccessible arguments",
                self.pid
            );
            if is_guard_runner(&self.cmd) {
                return Ok(Role::GuardRunner);
            }
        }
        Ok(Role::Other)
    }
}

fn snapshot(system: &mut System, pid: Pid) -> eyre::Result<ProcessSnapshot> {
    let refreshed = system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    ensure!(refreshed != 0, "could not inspect ancestor process {pid}");
    let process = system
        .process(pid)
        .ok_or_else(|| eyre::eyre!("ancestor process {pid} disappeared"))?;
    ensure!(
        process.start_time() != 0,
        "ancestor process {pid} has an inaccessible start time"
    );
    let exe = process
        .exe()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| eyre::eyre!("ancestor process {pid} has an inaccessible executable"))?;
    Ok(ProcessSnapshot {
        pid,
        parent: process.parent(),
        start: process.start_time(),
        exe: canonical_executable(exe)
            .with_context(|| format!("could not identify ancestor process {pid}"))?,
        cmd: process.cmd().to_vec(),
        cwd: process
            .cwd()
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf),
        environ: process.environ().to_vec(),
    })
}

fn canonical_executable(path: &Path) -> eyre::Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "process executable is not an absolute path: {}",
        path.display()
    );
    path.canonicalize()
        .with_context(|| format!("could not resolve executable {}", path.display()))
}

fn is_root(pid: Pid) -> bool {
    pid.as_u32() == 0 || (cfg!(unix) && pid.as_u32() == 1)
}

fn is_cargo_name(exe: &Path) -> bool {
    exe.file_name().is_some_and(|name| {
        name == "cargo"
            || (cfg!(windows) && name.as_encoded_bytes().eq_ignore_ascii_case(b"cargo.exe"))
    })
}

fn is_guard_runner(args: &[OsString]) -> bool {
    let mut args = args.iter().skip(1);
    let mut first = args.next();
    if first.is_some_and(|arg| arg == "guard") {
        first = args.next();
    }
    first.is_some_and(|arg| arg != "review" && !arg.as_encoded_bytes().starts_with(b"-"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Cargo,
    GuardRunner,
    Other,
}

#[derive(Default)]
struct CargoSelection {
    nearest: Option<usize>,
    policy: Option<usize>,
    owner_seen: bool,
}

impl CargoSelection {
    fn observe(&mut self, role: Role, index: usize, is_owner: bool) -> eyre::Result<()> {
        if is_owner {
            ensure!(
                role == Role::GuardRunner,
                "CARGO_GUARD_POLICY_PID does not identify a live cargo-guard build-script runner ancestor"
            );
            ensure!(
                self.nearest.is_some(),
                "policy owner encountered before the invoking Cargo"
            );
            self.owner_seen = true;
        }
        if role == Role::Cargo {
            self.nearest.get_or_insert(index);
            if self.owner_seen {
                self.policy = Some(index);
            }
        }
        Ok(())
    }

    fn complete(&self, needs_owner: bool) -> bool {
        self.nearest.is_some() && (!needs_owner || self.policy.is_some())
    }

    fn finish(self, needs_owner: bool) -> eyre::Result<(usize, Option<usize>)> {
        let nearest = self.nearest.ok_or_else(|| {
            eyre::eyre!(
                "no live invoking Cargo process found; standalone runner invocation is unsupported"
            )
        })?;
        if needs_owner {
            ensure!(
                self.owner_seen,
                "CARGO_GUARD_POLICY_PID does not identify an accessible ancestor"
            );
            ensure!(
                self.policy.is_some(),
                "policy owner has no live invoking Cargo ancestor"
            );
        }
        Ok((nearest, self.policy))
    }
}

fn parse_policy_owner(value: &OsStr, current: Pid) -> eyre::Result<u32> {
    let pid = value
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| eyre::eyre!("CARGO_GUARD_POLICY_PID is not a valid process ID"))?;
    ensure!(
        !is_root(Pid::from_u32(pid)) && pid != current.as_u32(),
        "CARGO_GUARD_POLICY_PID must identify a live ancestor guard runner"
    );
    Ok(pid)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CargoArguments {
    manifest: Option<PathBuf>,
    forwarded: Vec<OsString>,
}

fn metadata(process: &ProcessSnapshot, cargo: &Path) -> eyre::Result<Metadata> {
    ensure!(
        process.exe == cargo,
        "Cargo process {} uses a different executable from CARGO; cross-toolchain guarded ancestry is unsupported",
        process.pid
    );
    ensure!(
        !process.environ.is_empty(),
        "Cargo process {} has an inaccessible environment",
        process.pid
    );
    let cwd = process
        .cwd
        .as_deref()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            eyre::eyre!(
                "Cargo process {} has an inaccessible or invalid working directory",
                process.pid
            )
        })?;
    let expanded = expand_aliases(process, cwd)?;
    let arguments = parse_cargo_args(&expanded)
        .with_context(|| format!("could not interpret Cargo process {}", process.pid))?;
    let mut builder = MetadataCommand::new();
    builder.cargo_path(&process.exe).current_dir(cwd).no_deps();
    if let Some(manifest) = &arguments.manifest {
        builder.manifest_path(cwd.join(manifest));
    }
    let output = builder
        .cargo_command()
        .env_clear()
        .envs(process.environment())
        .arg("--offline")
        .args(&arguments.forwarded)
        .output()
        .with_context(|| format!("could not run Cargo metadata in {}", cwd.display()))?;
    ensure!(
        output.status.success(),
        "Cargo metadata failed for process {} in {}: {}",
        process.pid,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout =
        std::str::from_utf8(&output.stdout).wrap_err("Cargo metadata returned non-UTF-8 JSON")?;
    let json = stdout
        .lines()
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| eyre::eyre!("Cargo metadata returned no JSON"))?;
    MetadataCommand::parse(json).wrap_err("could not parse invoking Cargo workspace metadata")
}

fn is_workspace_command(command: &OsStr) -> bool {
    command.to_str().is_some_and(|name| {
        matches!(
            name,
            "build" | "check" | "test" | "run" | "bench" | "doc" | "rustc" | "rustdoc" | "fix"
        )
    })
}

#[derive(Default, Deserialize)]
struct Aliases {
    #[serde(default)]
    alias: BTreeMap<String, toml::Value>,
    #[serde(default)]
    unstable: BTreeMap<String, toml::Value>,
}

impl ProcessSnapshot {
    fn environment(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.environ.iter().filter_map(|entry| {
            let bytes = entry.as_encoded_bytes();
            let separator = bytes.iter().position(|byte| *byte == b'=')?;
            if separator == 0 {
                return None;
            }
            // ASCII '=' separates two valid encoded-byte strings.
            Some(unsafe {
                (
                    OsStr::from_encoded_bytes_unchecked(&bytes[..separator]),
                    OsStr::from_encoded_bytes_unchecked(&bytes[separator + 1..]),
                )
            })
        })
    }
}

impl Aliases {
    fn load(process: &ProcessSnapshot, cwd: &Path) -> eyre::Result<Self> {
        // Cargo resolves aliases before applying command-line --config or -Z flags.
        // Let its own loader handle config hierarchy, includes, and array merging.
        let output = Command::new(&process.exe)
            .current_dir(cwd)
            .env_clear()
            .envs(process.environment())
            .args(["-Zunstable-options", "config", "get", "--format=toml"])
            .output()
            .wrap_err("could not read invoking Cargo alias configuration")?;
        ensure!(
            output.status.success(),
            "Cargo could not read alias configuration: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let config =
            std::str::from_utf8(&output.stdout).wrap_err("Cargo configuration is not UTF-8")?;
        // Keep configuration values out of diagnostics.
        toml::from_str(config).map_err(|_| eyre::eyre!("could not parse Cargo alias configuration"))
    }

    fn resolve(
        &self,
        name: &str,
        process: &ProcessSnapshot,
        advanced_env: bool,
    ) -> eyre::Result<Vec<OsString>> {
        let env_key = format!(
            "CARGO_ALIAS_{}",
            name.replace(['-', '.'], "_").to_uppercase()
        );
        let environment = process
            .environment()
            .find(|(key, _)| *key == OsStr::new(&env_key))
            .map(|(_, value)| {
                value
                    .to_str()
                    .ok_or_else(|| eyre::eyre!("Cargo alias environment value is not UTF-8"))
            })
            .transpose()?;
        let mut args: Vec<OsString> = match self.alias.get(name) {
            Some(toml::Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value.as_str().map(OsString::from).ok_or_else(|| {
                        eyre::eyre!("Cargo alias {name} contains a non-string value")
                    })
                })
                .collect::<eyre::Result<_>>()?,
            Some(toml::Value::String(value)) => {
                return Ok(environment
                    .unwrap_or(value)
                    .split_whitespace()
                    .map(OsString::from)
                    .collect());
            }
            Some(_) => eyre::bail!("Cargo alias {name} is not a string or array"),
            None => Vec::new(),
        };
        if let Some(value) = environment {
            if advanced_env
                && self.alias.get(name).is_some_and(toml::Value::is_array)
                && value.starts_with('[')
                && value.ends_with(']')
            {
                let values = toml::de::ValueDeserializer::parse(value)
                    .and_then(Vec::<String>::deserialize)
                    .map_err(|_| eyre::eyre!("could not parse Cargo alias environment array"))?;
                args.extend(values.into_iter().map(OsString::from));
            } else {
                args.extend(value.split_whitespace().map(OsString::from));
            }
        } else if !self.alias.contains_key(name) {
            let builtin = match name {
                "b" => "build",
                "c" => "check",
                "d" => "doc",
                "r" => "run",
                "t" => "test",
                _ => {
                    eyre::bail!("Cargo command {name} does not select a supported build workspace")
                }
            };
            args.push(builtin.into());
        }
        Ok(args)
    }
}

fn expand_aliases<'a>(
    process: &'a ProcessSnapshot,
    cwd: &Path,
) -> eyre::Result<Cow<'a, [OsString]>> {
    let (first, changed_directory) = command_index(&process.cmd, 1)?;
    if is_workspace_command(&process.cmd[first]) {
        return Ok(Cow::Borrowed(&process.cmd));
    }
    let aliases = Aliases::load(process, cwd)?;
    // -C reloads unstable config before aliases. Normal invocation loads it afterward.
    let advanced_env = changed_directory
        && process
            .environment()
            .find(|(key, _)| *key == "CARGO_UNSTABLE_ADVANCED_ENV")
            .map(|(_, value)| value == "true")
            .unwrap_or_else(|| {
                aliases
                    .unstable
                    .get("advanced-env")
                    .and_then(toml::Value::as_bool)
                    == Some(true)
            });
    let mut args = Cow::Borrowed(&process.cmd[first..]);
    let mut seen = HashSet::new();
    loop {
        let name = args[0]
            .to_str()
            .ok_or_else(|| eyre::eyre!("Cargo command is not UTF-8"))?;
        ensure!(seen.insert(name.to_owned()), "Cargo alias cycle at {name}");
        let mut expanded = aliases.resolve(name, process, advanced_env)?;
        ensure!(!expanded.is_empty(), "Cargo alias {name} is empty");
        expanded.extend_from_slice(&args[1..]);
        let (command, _) = command_index(&expanded, 0)?;
        if is_workspace_command(&expanded[command]) {
            let mut result = process.cmd[..first].to_vec();
            result.extend(expanded);
            return Ok(Cow::Owned(result));
        }
        // Cargo retains the original global arguments, not intermediate alias prefixes.
        args = Cow::Owned(expanded.into_iter().skip(command).collect());
    }
}

fn command_index(args: &[OsString], mut index: usize) -> eyre::Result<(usize, bool)> {
    ensure!(!args.is_empty(), "Cargo arguments are inaccessible");
    let mut changed_directory = false;
    while let Some(arg) = args.get(index) {
        let current = index;
        index += 1;
        ensure!(arg != "--", "Cargo command is missing before --");
        if arg == "--config" || arg == "--color" {
            take_value(args, &mut index, arg)?;
        } else if let Some((option, value)) = short_value_option(arg) {
            changed_directory |= option == "-C";
            if value.is_empty() {
                take_value(args, &mut index, OsStr::new(option))?;
            }
        } else if !arg.as_encoded_bytes().starts_with(b"-") {
            return Ok((current, changed_directory));
        }
    }
    eyre::bail!("Cargo command is missing")
}

fn short_value_option(arg: &OsStr) -> Option<(&'static str, &OsStr)> {
    let bytes = arg.as_encoded_bytes();
    if bytes.first() != Some(&b'-') || bytes.get(1) == Some(&b'-') {
        return None;
    }
    for (index, byte) in bytes.iter().enumerate().skip(1) {
        let option = match byte {
            b'v' | b'q' | b'r' | b'h' | b'V' => continue,
            b'Z' => "-Z",
            b'C' => "-C",
            b'p' => "-p",
            b'j' => "-j",
            b'F' => "-F",
            _ => return None,
        };
        // Short option characters are ASCII encoded-byte boundaries.
        return Some((option, unsafe {
            OsStr::from_encoded_bytes_unchecked(&bytes[index + 1..])
        }));
    }
    None
}

fn parse_cargo_args(args: &[OsString]) -> eyre::Result<CargoArguments> {
    let (command, _) = command_index(args, 1)?;
    ensure!(
        is_workspace_command(&args[command]),
        "Cargo command does not select a supported build workspace: {}",
        args[command].to_string_lossy()
    );
    let mut result = CargoArguments::default();
    let mut index = 1;
    while let Some(arg) = args.get(index) {
        index += 1;
        if arg == "--" {
            break;
        }
        if let Some(value) = strip_prefix(arg, "--manifest-path=") {
            set_manifest(&mut result, value)?;
        } else if arg == "--manifest-path" {
            set_manifest(&mut result, take_value(args, &mut index, arg)?)?;
        } else if let Some(value) = strip_prefix(arg, "--config=") {
            ensure!(!value.is_empty(), "empty Cargo --config value");
            result
                .forwarded
                .extend([OsString::from("--config"), value.to_owned()]);
        } else if arg == "--config" {
            result
                .forwarded
                .extend([arg.clone(), take_value(args, &mut index, arg)?.to_owned()]);
        } else if let Some((option, value)) = short_value_option(arg) {
            let value = if value.is_empty() {
                take_value(args, &mut index, OsStr::new(option))?
            } else {
                value
            };
            if option == "-Z" {
                result
                    .forwarded
                    .extend([OsString::from("-Z"), value.to_owned()]);
            }
            // Cargo has already applied -C. Other build options do not select a workspace.
        }
    }
    Ok(result)
}

fn take_value<'a>(
    args: &'a [OsString],
    index: &mut usize,
    option: &OsStr,
) -> eyre::Result<&'a OsStr> {
    let value = args
        .get(*index)
        .filter(|value| !value.is_empty() && *value != "--")
        .ok_or_else(|| {
            eyre::eyre!(
                "Cargo option {} is missing its value",
                option.to_string_lossy()
            )
        })?;
    let negative_jobs = (option == "-j" || option == "--jobs")
        && value
            .to_str()
            .is_some_and(|value| value.parse::<i32>().is_ok());
    ensure!(
        !value.as_encoded_bytes().starts_with(b"-") || negative_jobs,
        "Cargo option {} has an ambiguous value: {}",
        option.to_string_lossy(),
        value.to_string_lossy()
    );
    *index += 1;
    Ok(value)
}

fn set_manifest(result: &mut CargoArguments, value: &OsStr) -> eyre::Result<()> {
    ensure!(!value.is_empty(), "Cargo manifest path is empty");
    ensure!(
        result.manifest.is_none(),
        "multiple Cargo manifest paths are ambiguous"
    );
    result.manifest = Some(PathBuf::from(value));
    Ok(())
}

fn strip_prefix<'a>(value: &'a OsStr, prefix: &str) -> Option<&'a OsStr> {
    let remainder = value.as_encoded_bytes().strip_prefix(prefix.as_bytes())?;
    // The removed prefix is ASCII, so this is a valid encoded-byte boundary.
    Some(unsafe { OsStr::from_encoded_bytes_unchecked(remainder) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> eyre::Result<CargoArguments> {
        parse_cargo_args(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn manifest_forms_and_explicit_configuration_are_preserved() {
        for args in [
            vec![
                "cargo",
                "--config",
                "source.crates-io.replace-with='local'",
                "build",
                "--manifest-path",
                "../app/Cargo.toml",
            ],
            vec![
                "cargo",
                "build",
                "--config=source.crates-io.replace-with='local'",
                "--manifest-path=../app/Cargo.toml",
            ],
        ] {
            let args = parse(&args).unwrap();
            assert_eq!(args.manifest, Some(PathBuf::from("../app/Cargo.toml")));
            assert_eq!(
                args.forwarded,
                ["--config", "source.crates-io.replace-with='local'"]
            );
        }
        assert!(parse(&["cargo", "build", "--manifest-path=a", "--manifest-path=b"]).is_err());
    }

    #[test]
    fn forwarded_arguments_cannot_override_workspace_selection() {
        for command in ["run", "test", "bench", "rustc", "rustdoc"] {
            let args = parse(&[
                "cargo",
                command,
                "--manifest-path=app/Cargo.toml",
                "--",
                "--manifest-path",
                "untrusted/Cargo.toml",
                "--config=bad",
            ])
            .unwrap();
            assert_eq!(args.manifest, Some(PathBuf::from("app/Cargo.toml")));
            assert!(args.forwarded.is_empty());
        }
    }

    #[test]
    fn common_option_values_are_not_mistaken_for_commands_or_manifests() {
        let args = parse(&[
            "cargo",
            "-vv",
            "--color",
            "never",
            "-Zunstable-options",
            "-C",
            "already-applied",
            "test",
            "-p",
            "package",
            "-j4",
            "-Fone,two",
            "--target-dir",
            "artifacts",
            "--profile=dev",
            "--test",
            "integration",
            "filter",
            "--no-fail-fast",
        ])
        .unwrap();
        assert!(args.manifest.is_none());
        assert_eq!(args.forwarded, ["-Z", "unstable-options"]);
        for command in [
            "build", "check", "test", "run", "bench", "doc", "rustc", "rustdoc",
        ] {
            assert!(parse(&["cargo", command]).is_ok());
        }
    }

    #[test]
    fn invalid_workspace_selection_fails_closed() {
        for command in [
            "b",
            "c",
            "t",
            "r",
            "custom-build",
            "install",
            "package",
            "publish",
            "script",
            "guard",
        ] {
            assert!(parse(&["cargo", command]).is_err());
        }
        for args in [
            vec!["cargo", "build", "--manifest-path"],
            vec![
                "cargo",
                "build",
                "-p",
                "--manifest-path",
                "other/Cargo.toml",
            ],
            vec!["cargo", "build", "--config="],
            vec!["cargo", "--", "build"],
        ] {
            assert!(parse(&args).is_err());
        }
    }

    #[test]
    fn unrelated_build_flags_and_positionals_do_not_restrict_cargo() {
        let args = parse(&[
            "cargo",
            "-vvZunstable-options",
            "test",
            "--new-cargo-option",
            "value",
            "filter",
            "--manifest-path=app/Cargo.toml",
            "--",
            "--config=ignored",
        ])
        .unwrap();
        assert_eq!(args.manifest, Some(PathBuf::from("app/Cargo.toml")));
        assert_eq!(args.forwarded, ["-Z", "unstable-options"]);
    }

    #[test]
    fn policy_owner_selects_its_cargo_and_stops_before_unrelated_ancestors() {
        use Role::{Cargo, GuardRunner, Other};
        fn select(roles: &[Role], owner: Option<usize>) -> eyre::Result<(usize, Option<usize>)> {
            let mut selection = CargoSelection::default();
            for (index, role) in roles.iter().enumerate() {
                selection.observe(*role, index, owner == Some(index))?;
                if selection.complete(owner.is_some()) {
                    break;
                }
            }
            selection.finish(owner.is_some())
        }
        assert_eq!(select(&[Cargo, Other, Cargo], None).unwrap(), (0, None));
        assert_eq!(
            select(&[Cargo, GuardRunner, Cargo], None).unwrap(),
            (0, None)
        );
        assert_eq!(
            select(
                &[Other, Cargo, Other, GuardRunner, Cargo, Other, Cargo],
                Some(3)
            )
            .unwrap(),
            (1, Some(4))
        );
        assert_eq!(
            select(
                &[Cargo, GuardRunner, Cargo, GuardRunner, Other, Cargo],
                Some(3)
            )
            .unwrap(),
            (0, Some(5))
        );
        assert!(select(&[Cargo, GuardRunner, Other], Some(1)).is_err());
        assert!(select(&[GuardRunner, Cargo], Some(0)).is_err());
        assert!(select(&[Cargo, Other, Cargo], Some(1)).is_err());
        assert!(select(&[Cargo, Other], Some(9)).is_err());
        assert!(select(&[Other], None).is_err());
    }

    #[test]
    fn invalid_or_self_referential_policy_owner_is_rejected() {
        for value in ["", "invalid", "0", "99", "4294967296"] {
            assert!(parse_policy_owner(OsStr::new(value), Pid::from_u32(99)).is_err());
        }
        assert_eq!(
            parse_policy_owner(OsStr::new("42"), Pid::from_u32(99)).unwrap(),
            42
        );
    }

    #[test]
    fn review_and_help_are_not_guard_runner_boundaries() {
        for args in [
            vec!["guard", "review"],
            vec!["guard", "guard", "review"],
            vec!["guard", "--help"],
            vec!["guard"],
        ] {
            assert!(!is_guard_runner(
                &args.into_iter().map(OsString::from).collect::<Vec<_>>()
            ));
        }
        for args in [
            vec!["guard", "/target/build-script"],
            vec!["guard", "guard", "/target/build-script", "review"],
        ] {
            assert!(is_guard_runner(
                &args.into_iter().map(OsString::from).collect::<Vec<_>>()
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_manifest_and_config_paths_are_preserved() {
        use std::os::unix::ffi::OsStringExt;
        let manifest = OsString::from_vec(b"dir-\xff/Cargo.toml".to_vec());
        let config = OsString::from_vec(b"config-\xfe.toml".to_vec());
        let mut manifest_option = OsString::from("--manifest-path=");
        manifest_option.push(&manifest);
        let mut config_option = OsString::from("--config=");
        config_option.push(&config);
        let args = parse_cargo_args(&[
            "cargo".into(),
            "build".into(),
            manifest_option,
            config_option,
        ])
        .unwrap();
        assert_eq!(args.manifest, Some(PathBuf::from(manifest)));
        assert_eq!(args.forwarded, [OsString::from("--config"), config]);
    }
}
