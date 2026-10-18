use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const GLOBAL_ENV: &str = "CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH";

struct Fixture {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    cargo_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let cargo_home = temp.path().join("cargo-home");
        fs::create_dir_all(&cargo_home).unwrap();
        write(
            &workspace.join("Cargo.toml"),
            "[package]\nname = 'empty-review'\nversion = '1.0.0'\nedition = '2024'\n[workspace]\n",
        );
        write(&workspace.join("src/lib.rs"), "");
        Self {
            temp,
            workspace,
            cargo_home,
        }
    }

    fn global(&self) -> PathBuf {
        self.temp.path().join("external/custom-approvals.toml")
    }

    fn default_global(&self) -> PathBuf {
        self.cargo_home.join("approved-builds.toml")
    }

    fn workspace_approvals(&self) -> PathBuf {
        self.workspace.join("approved-builds.toml")
    }

    fn home_config(&self, contents: &str) {
        write(&self.cargo_home.join("config.toml"), contents);
    }

    fn project_config(&self, contents: &str) {
        write(&self.workspace.join(".cargo/config.toml"), contents);
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-guard"));
        command.current_dir(&self.workspace).stdin(Stdio::null());
        for (key, _) in std::env::vars_os() {
            if key == "CARGO" || key.to_string_lossy().starts_with("CARGO_") {
                command.env_remove(key);
            }
        }
        for key in [
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "RUSTC_BOOTSTRAP",
        ] {
            command.env_remove(key);
        }
        command
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TARGET_DIR", self.temp.path().join("target"))
            .env("CARGO_NET_OFFLINE", "true");
        command
    }

    fn review(&self, global: bool) -> Command {
        let mut command = self.command();
        command.arg("review");
        if global {
            command.arg("--global");
        }
        command
    }

    fn assert_no_approvals_written(&self, other_paths: &[&Path]) {
        for path in [self.default_global(), self.workspace_approvals()]
            .iter()
            .map(PathBuf::as_path)
            .chain(other_paths.iter().copied())
        {
            assert!(
                !path.exists(),
                "unexpected approval file: {}",
                path.display()
            );
            let mut lock = path.as_os_str().to_owned();
            lock.push(".lock");
            assert!(
                !Path::new(&lock).exists(),
                "unexpected approval lock: {lock:?}"
            );
        }
    }
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn env_config(path: &Path, force: bool) -> String {
    let value = toml::Value::String(path.to_str().unwrap().to_owned());
    format!("[env]\n{GLOBAL_ENV} = {{ value = {value}, force = {force} }}\n")
}

fn diagnostics(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn reported_destination(output: &Output) -> &Path {
    let text = diagnostics(output);
    assert!(output.status.success(), "{text}");
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| {
            let (_, path) = line.strip_prefix("Approvals (")?.split_once("): ")?;
            Some(Path::new(path))
        })
        .unwrap_or_else(|| panic!("missing approval destination\n{text}"))
}

fn resolved_destination(path: &Path) -> PathBuf {
    assert!(path.is_absolute(), "expected an absolute path: {path:?}");
    // Approval files and their immediate parents may not exist yet.
    let ancestor = path.ancestors().find(|ancestor| ancestor.exists()).unwrap();
    ancestor
        .canonicalize()
        .unwrap()
        .join(path.strip_prefix(ancestor).unwrap())
}

fn assert_destination(output: &Output, destination: &Path) {
    assert_eq!(
        resolved_destination(reported_destination(output)),
        resolved_destination(destination),
        "{}",
        diagnostics(output)
    );
}

#[test]
fn global_review_uses_custom_path_from_cargo_home_configuration() {
    let fixture = Fixture::new();
    let global = fixture.global();
    fixture.home_config(&env_config(&global, false));

    assert_destination(&fixture.review(true).output().unwrap(), &global);
    fixture.assert_no_approvals_written(&[&global]);
    assert!(!global.parent().unwrap().exists());
}

#[test]
fn project_configuration_overrides_cargo_home_configuration() {
    let fixture = Fixture::new();
    let home_global = fixture.global();
    let project_global = fixture.temp.path().join("project-policy.toml");
    fixture.home_config(&env_config(&home_global, false));
    fixture.project_config(&env_config(&project_global, false));

    assert_destination(&fixture.review(true).output().unwrap(), &project_global);
    fixture.assert_no_approvals_written(&[&home_global, &project_global]);
}

#[test]
fn relative_configuration_path_uses_declaring_project_base_not_invocation_directory() {
    let fixture = Fixture::new();
    fixture.project_config(&format!(
        "[env]\n{GLOBAL_ENV} = {{ value = 'policy/global.toml', relative = true }}\n"
    ));
    let invocation = fixture.workspace.join("nested/deeper");
    fs::create_dir_all(&invocation).unwrap();
    let global = fixture.workspace.join("policy/global.toml");

    let output = fixture
        .review(true)
        .current_dir(&invocation)
        .output()
        .unwrap();
    assert_destination(&output, &global);
    fixture.assert_no_approvals_written(&[
        &global,
        &invocation.join("policy/global.toml"),
        &fixture.workspace.join(".cargo/policy/global.toml"),
    ]);
}

#[test]
fn member_configuration_is_resolved_from_invocation_directory() {
    let fixture = Fixture::new();
    write(
        &fixture.workspace.join("Cargo.toml"),
        "[workspace]\nmembers = ['member']\nresolver = '2'\n",
    );
    let member = fixture.workspace.join("member");
    write(
        &member.join("Cargo.toml"),
        "[package]\nname = 'empty-member'\nversion = '1.0.0'\nedition = '2024'\n",
    );
    write(&member.join("src/lib.rs"), "");
    fixture.home_config(&env_config(&fixture.global(), false));
    write(
        &member.join(".cargo/config.toml"),
        &format!("[env]\n{GLOBAL_ENV} = {{ value = 'member-policy.toml', relative = true }}\n"),
    );
    let global = member.join("member-policy.toml");

    let output = fixture.review(true).current_dir(&member).output().unwrap();
    assert_destination(&output, &global);
    fixture.assert_no_approvals_written(&[&global, &fixture.global()]);
}

#[test]
fn exported_path_wins_unless_cargo_configuration_forces_its_value() {
    for force in [false, true] {
        let fixture = Fixture::new();
        let configured = fixture.global();
        let exported = fixture.temp.path().join("exported.toml");
        fixture.home_config(&env_config(&configured, force));

        let output = fixture
            .review(true)
            .env(GLOBAL_ENV, &exported)
            .output()
            .unwrap();
        assert_destination(&output, if force { &configured } else { &exported });
        fixture.assert_no_approvals_written(&[&configured, &exported]);
    }
}

#[test]
fn global_review_defaults_to_cargo_home_without_creating_files() {
    let fixture = Fixture::new();
    assert_destination(
        &fixture.review(true).output().unwrap(),
        &fixture.default_global(),
    );
    fixture.assert_no_approvals_written(&[]);
}

#[test]
fn relative_cargo_home_default_is_resolved_against_invocation_directory() {
    let fixture = Fixture::new();
    let output = fixture
        .review(true)
        .env("CARGO_HOME", "../cargo-home")
        .output()
        .unwrap();
    assert_destination(&output, &fixture.default_global());
    fixture.assert_no_approvals_written(&[]);
}

#[test]
fn ordinary_review_remains_usable_with_invalid_global_document() {
    let fixture = Fixture::new();
    let global = fixture.global();
    fixture.home_config(&env_config(&global, false));
    let invalid_document = "not a valid approval document [";
    write(&global, invalid_document);

    assert_destination(
        &fixture.review(false).output().unwrap(),
        &fixture.workspace_approvals(),
    );
    assert_eq!(fs::read_to_string(&global).unwrap(), invalid_document);
    fixture.assert_no_approvals_written(&[]);
}

#[test]
fn invalid_effective_global_paths_fail_without_default_writes() {
    for value in ["", "relative/approvals.toml", "~/approved-builds.toml"] {
        for from_config in [false, true] {
            let fixture = Fixture::new();
            let mut command = fixture.review(true);
            if from_config {
                fixture.home_config(&env_config(Path::new(value), false));
            } else {
                command.env(GLOBAL_ENV, value);
            }
            let output = command.output().unwrap();
            let text = diagnostics(&output);
            assert!(!output.status.success(), "{text}");
            assert!(text.contains(GLOBAL_ENV), "{text}");
            fixture.assert_no_approvals_written(&[
                &fixture.workspace.join("relative/approvals.toml"),
                &fixture.workspace.join("~/approved-builds.toml"),
            ]);
        }
    }
}

#[test]
fn invalid_cargo_configuration_fails_without_default_writes() {
    for contents in [
        "[env\n",
        "[env]\nCARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH = 42\n",
    ] {
        let fixture = Fixture::new();
        fixture.home_config(contents);
        let output = fixture.review(true).output().unwrap();
        assert!(!output.status.success(), "{}", diagnostics(&output));
        fixture.assert_no_approvals_written(&[]);
    }
}

#[cfg(unix)]
#[test]
fn configured_global_approval_does_not_authorize_standalone_runner() {
    let fixture = Fixture::new();
    let approvals = "[approved]\nempty-review = '1.0.0'\n";
    write(&fixture.default_global(), approvals);
    let marker = fixture.temp.path().join("script-ran");
    let output = fixture
        .command()
        .env(GLOBAL_ENV, fixture.default_global())
        .env("CARGO_MANIFEST_PATH", fixture.workspace.join("Cargo.toml"))
        .env("CARGO_PKG_NAME", "empty-review")
        .env("CARGO_PKG_VERSION", "1.0.0")
        .env("OUT_DIR", fixture.temp.path().join("out"))
        .args(["/bin/sh", "-c", "printf executed > \"$1\"", "script"])
        .arg(&marker)
        .output()
        .unwrap();
    let text = diagnostics(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        !marker.exists(),
        "runner executed without an invoking Cargo build\n{text}"
    );
    assert_eq!(
        fs::read_to_string(fixture.default_global()).unwrap(),
        approvals
    );
    assert!(!fixture.workspace_approvals().exists());
}
