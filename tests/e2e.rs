use std::{
    fmt::Write as _,
    fs,
    io::{Read, Write as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

const TOOLCHAIN: &str = "nightly";

#[test]
fn unapproved_dependency_build_script_does_not_execute() {
    let fixture = Fixture::new("[approved]\n");
    let marker = fixture.marker();
    assert!(
        !marker
            .try_exists()
            .unwrap_or_else(|error| panic!("could not inspect {}: {error}", marker.display())),
        "marker already exists before building: {}",
        marker.display()
    );

    let output = fixture.build();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let diagnostics = format!(
        "rustup run {TOOLCHAIN} cargo build --offline --color never\nstatus: {}\nstdout:\n{}\nstderr:\n{stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(!output.status.success(), "{diagnostics}");
    assert!(
        stderr.contains("build script for guarded-dep v1.0.0 is not approved: no approval entry"),
        "missing Cargo Guard rejection\n{diagnostics}"
    );
    assert!(
        !marker.try_exists().unwrap_or_else(|error| {
            panic!(
                "could not inspect {}: {error}\n{diagnostics}",
                marker.display()
            )
        }),
        "unapproved build script executed\n{diagnostics}"
    );
}

#[test]
fn approved_dependency_build_script_executes() {
    let fixture = Fixture::new("[approved]\nguarded-dep = \"1.0.0\"\n");
    let output = fixture.build();
    let diagnostics = format!(
        "rustup run {TOOLCHAIN} cargo build --offline --color never\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{diagnostics}");
    let marker = fixture.marker();
    let contents = fs::read(&marker).unwrap_or_else(|error| {
        panic!(
            "could not read {}: {error}\n{diagnostics}",
            marker.display()
        )
    });
    assert_eq!(contents, b"executed\n", "{diagnostics}");
}

#[test]
fn cargo_directives_and_generated_code_survive_the_runner() {
    let fixture = Fixture::new("[approved]\nguarded-dep = '1.0.0'\n");
    let root = fixture.temp.path().join("workspace");
    write(
        &root,
        "dep/build.rs",
        r#"fn main() {
    let out_dir = std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").expect("missing output directory"),
    );
    std::fs::write(
        out_dir.join("generated.rs"),
        "pub const GENERATED_VALUE: u32 = 42;\n",
    )
    .expect("could not write generated code");
    println!("cargo::rustc-env=CARGO_GUARD_E2E_DIRECTIVE_VALUE=from build script");
}
"#,
    );
    write(
        &root,
        "dep/src/lib.rs",
        r#"include!(concat!(env!("OUT_DIR"), "/generated.rs"));
pub const DIRECTIVE_VALUE: &str = env!("CARGO_GUARD_E2E_DIRECTIVE_VALUE");
"#,
    );
    write(
        &root,
        "src/main.rs",
        r#"fn main() {
    println!(
        "{}:{}",
        guarded_dep::GENERATED_VALUE,
        guarded_dep::DIRECTIVE_VALUE,
    );
}
"#,
    );

    let output = fixture.cargo(
        "workspace",
        false,
        &fixture.marker(),
        &["run", "--offline", "--quiet", "--color", "never"],
    );
    assert_success(&output);
    assert_eq!(output.stdout, b"42:from build script\n");
}

#[test]
fn blanket_approval_survives_version_and_source_changes_but_is_package_specific() {
    let policy = "[approved]\nguarded-dep = 'approved'\n";
    let fixture = Fixture::new(policy);
    let root = fixture.temp.path().join("workspace");
    let script = r#"fn main() {
    std::fs::write(
        std::env::var_os("CARGO_GUARD_E2E_MARKER").expect("missing marker path"),
        format!("{}:{}\n", env!("CARGO_PKG_VERSION"), include_str!("helper.txt")),
    )
    .expect("could not write execution marker");
}
"#;
    write(&root, "dep/build.rs", script);
    write(&root, "dep/helper.txt", "original");

    assert_success(&fixture.build());
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"1.0.0:original\n");
    fs::remove_file(fixture.marker()).unwrap();

    let manifest = fs::read_to_string(root.join("dep/Cargo.toml")).unwrap();
    let manifest = manifest.replace("version = \"1.0.0\"", "version = \"2.0.0\"");
    write(&root, "dep/Cargo.toml", &manifest);
    assert_success(&fixture.build());
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"2.0.0:original\n");
    fs::remove_file(fixture.marker()).unwrap();

    write(&root, "dep/helper.txt", "changed");
    assert_success(&fixture.build());
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"2.0.0:changed\n");
    fs::remove_file(fixture.marker()).unwrap();

    write(
        &root,
        "dep/build.rs",
        &script.replace("format!(\"{}:{}", "format!(\"changed:{}:{}"),
    );
    assert_success(&fixture.build());
    assert_eq!(
        fs::read(fixture.marker()).unwrap(),
        b"changed:2.0.0:changed\n"
    );
    fs::remove_file(fixture.marker()).unwrap();

    // Reuse the same script and inputs under another package name: a blanket
    // entry authorizes only its named package, not every dependency.
    write(
        &root,
        "dep/Cargo.toml",
        &manifest.replace("guarded-dep", "other-dep"),
    );
    let consumer = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    write(
        &root,
        "Cargo.toml",
        &consumer.replace("guarded-dep", "other-dep"),
    );
    assert_rejected(&fixture.build(), &fixture.marker(), "other-dep v2.0.0");
    assert_eq!(
        fs::read_to_string(root.join("approved-builds.toml")).unwrap(),
        policy
    );
}

#[test]
fn global_blanket_approval_yields_to_workspace_policy() {
    let fixture = Fixture::global();
    let root = fixture.temp.path().join("workspace");
    let marker = fixture.workspace_marker("workspace");
    fixture.approve_global("[approved]\nguarded-dep = 'approved'\n");

    assert_success(&fixture.build_workspace("workspace", false));
    assert_eq!(fs::read(&marker).unwrap(), b"executed\n");
    fs::remove_file(&marker).unwrap();

    write(
        &root,
        "approved-builds.toml",
        "[approved]\nguarded-dep = '9.0.0'\n",
    );
    fixture.clean_dependency("workspace");
    let rejected = fixture.build_workspace("workspace", true);
    assert_rejected(&rejected, &marker, "guarded-dep v1.0.0");
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        stderr.contains(root.join("approved-builds.toml").to_str().unwrap()),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(fixture.global_path()).unwrap(),
        "[approved]\nguarded-dep = 'approved'\n"
    );

    // Workspace precedence also holds when the workspace, rather than the
    // global policy, contains the blanket approval.
    fixture.approve_global("[approved]\nguarded-dep = '9.0.0'\n");
    write(
        &root,
        "approved-builds.toml",
        "[approved]\nguarded-dep = 'approved'\n",
    );
    fixture.clean_dependency("workspace");
    assert_success(&fixture.build_workspace("workspace", true));
    assert_eq!(fs::read(&marker).unwrap(), b"executed\n");
    assert_eq!(
        fs::read_to_string(fixture.global_path()).unwrap(),
        "[approved]\nguarded-dep = '9.0.0'\n"
    );
}

#[test]
fn fingerprint_approval_checks_both_hashes_but_ignores_version_hints() {
    let fixture = Fixture::new("[approved]\nguarded-dep = 'legacy-hash'\n");
    let root = fixture.temp.path().join("workspace");
    let script_path = root.join("dep/build.rs");
    let script = format!(
        "{}\nconst _: &str = include_str!(\"helper.txt\");\n",
        fs::read_to_string(&script_path).unwrap()
    );
    fs::write(&script_path, &script).unwrap();
    write(&root, "dep/helper.txt", "original");

    let rejected = fixture.build();
    let original = rejected_fingerprint(&rejected);
    assert!(!fixture.marker().exists());
    let (hashes, hint) = original.split_once('#').unwrap();
    let (script_hash, inputs_hash) = hashes.split_once('+').unwrap();
    assert_eq!(hint, "1.0.0");
    let mut audited_hash = String::with_capacity(64);
    for byte in Sha256::digest(script.as_bytes()) {
        write!(&mut audited_hash, "{byte:02x}").unwrap();
    }
    assert_eq!(script_hash, audited_hash);

    write(
        &root,
        "approved-builds.toml",
        &format!("[approved]\nguarded-dep = '{hashes}#9.0.0+untrusted'\n"),
    );
    let approved = fixture.build();
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
    fs::remove_file(fixture.marker()).unwrap();

    write(&root, "dep/helper.txt", "changed");
    let helper_changed = rejected_fingerprint(&fixture.build());
    let (helper_hashes, _) = helper_changed.split_once('#').unwrap();
    let (same_script_hash, changed_inputs_hash) = helper_hashes.split_once('+').unwrap();
    assert_eq!(same_script_hash, script_hash);
    assert_ne!(changed_inputs_hash, inputs_hash);
    assert!(!fixture.marker().exists());

    fs::write(&script_path, format!("// changed entry point\n{script}")).unwrap();
    let script_changed = rejected_fingerprint(&fixture.build());
    let (script_hashes, _) = script_changed.split_once('#').unwrap();
    let (changed_script_hash, same_inputs_hash) = script_hashes.split_once('+').unwrap();
    assert_ne!(changed_script_hash, script_hash);
    assert_eq!(same_inputs_hash, changed_inputs_hash);
    assert!(!fixture.marker().exists());
}

#[test]
fn review_generated_fingerprint_approval_authorizes_a_real_build() {
    let fixture = Fixture::new("[approved]\n");
    let root = fixture.temp.path().join("workspace");
    write(
        &root,
        "dep/build.rs",
        r#"fn main() {
    std::fs::write(
        std::env::var_os("CARGO_GUARD_E2E_MARKER").expect("missing marker path"),
        include_str!("helper.txt"),
    )
    .expect("could not write execution marker");
}
"#,
    );
    write(&root, "dep/helper.txt", "reviewed contents\n");

    assert_rejected(&fixture.build(), &fixture.marker(), "guarded-dep v1.0.0");
    let mut review = fixture.command("workspace", false, &fixture.marker());
    review.args([
        env!("CARGO_BIN_EXE_cargo-guard"),
        "review",
        "guarded-dep@1.0.0",
    ]);
    approve_review(&review);
    assert!(
        !fixture.marker().exists(),
        "review executed the build script"
    );

    let policy_path = root.join("approved-builds.toml");
    let saved = fs::read_to_string(&policy_path).unwrap();

    assert_success(&fixture.build());
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"reviewed contents\n");
    fs::remove_file(fixture.marker()).unwrap();

    write(&root, "dep/helper.txt", "unreviewed contents\n");
    let changed = fixture.build();
    assert_rejected(&changed, &fixture.marker(), "guarded-dep v1.0.0");
    assert_eq!(fs::read_to_string(&policy_path).unwrap(), saved);
}

#[test]
fn global_version_approval_authorizes_two_rootless_workspaces() {
    let fixture = Fixture::global();
    for workspace in ["workspace", "workspace-b"] {
        let rejected = fixture.build_workspace(workspace, false);
        assert_rejected(
            &rejected,
            &fixture.workspace_marker(workspace),
            "guarded-dep v1.0.0",
        );
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains(fixture.global_path().to_str().unwrap()),
            "rejection did not name the configured global destination"
        );
    }
    assert!(!fixture.global_path().exists());

    fixture.approve_global("[approved]\nguarded-dep = '1.0.0'\n");
    for workspace in ["workspace", "workspace-b"] {
        assert_success(&fixture.build_workspace(workspace, false));
        assert_eq!(
            fs::read(fixture.workspace_marker(workspace)).unwrap(),
            b"executed\n"
        );
        assert!(
            !fixture
                .temp
                .path()
                .join(workspace)
                .join("approved-builds.toml")
                .exists()
        );
    }
}

#[test]
fn rootless_global_build_requires_workspace_member_approval() {
    let fixture = Fixture::global();
    let root = fixture.temp.path().join("workspace");
    write(
        &root,
        "build.rs",
        r#"fn main() {
    let marker = std::path::PathBuf::from(
        std::env::var_os("CARGO_GUARD_E2E_MARKER").expect("missing marker path"),
    ).with_extension("member");
    std::fs::write(marker, b"executed\n").expect("could not write member marker");
}
"#,
    );
    fixture.approve_global("[approved]\nguarded-dep = '1.0.0'\n");
    let member_marker = fixture
        .workspace_marker("workspace")
        .with_extension("member");
    let rejected = fixture.build_workspace("workspace", false);
    assert_rejected(&rejected, &member_marker, "consumer v0.1.0");

    fixture.approve_global("[approved]\nguarded-dep = '1.0.0'\nconsumer = '0.1.0'\n");
    assert_success(&fixture.build_workspace("workspace", false));
    assert_eq!(fs::read(member_marker).unwrap(), b"executed\n");
    assert!(!root.join("approved-builds.toml").exists());
}

#[test]
fn global_fingerprint_is_shared_but_workspace_policy_remains_authoritative() {
    let fixture = Fixture::global();
    fixture.approve_global("[approved]\nguarded-dep = 'legacy-hash'\n");
    let rejected = fixture.build_workspace("workspace", true);
    assert_rejected(
        &rejected,
        &fixture.workspace_marker("workspace"),
        "guarded-dep v1.0.0",
    );
    let fingerprint = rejected_fingerprint(&rejected);
    fixture.approve_global(&format!("[approved]\nguarded-dep = '{fingerprint}'\n"));

    for workspace in ["workspace", "workspace-b"] {
        assert_success(&fixture.build_workspace(workspace, true));
        assert_eq!(
            fs::read(fixture.workspace_marker(workspace)).unwrap(),
            b"executed\n"
        );
        assert!(
            !fixture
                .temp
                .path()
                .join(workspace)
                .join("approved-builds.toml")
                .exists()
        );
    }

    let second_root = fixture.temp.path().join("workspace-b");
    fs::remove_file(fixture.workspace_marker("workspace-b")).unwrap();
    write(&second_root, "dep/helper.txt", "changed");
    let changed = fixture.build_workspace("workspace-b", true);
    assert_rejected(
        &changed,
        &fixture.workspace_marker("workspace-b"),
        "guarded-dep v1.0.0",
    );
    assert_ne!(rejected_fingerprint(&changed), fingerprint);

    // The first workspace still matches the global fingerprint. Force Cargo to
    // invoke its runner again after installing an authoritative empty policy.
    let first_root = fixture.temp.path().join("workspace");
    write(&first_root, "approved-builds.toml", "[approved]\n");
    fs::remove_file(fixture.workspace_marker("workspace")).unwrap();
    fixture.clean_dependency("workspace");
    let authoritative = fixture.build_workspace("workspace", true);
    assert_rejected(
        &authoritative,
        &fixture.workspace_marker("workspace"),
        "guarded-dep v1.0.0",
    );
    let stderr = String::from_utf8_lossy(&authoritative.stderr);
    assert!(stderr.contains("no approval entry"), "{stderr}");
    assert!(
        stderr.contains(first_root.join("approved-builds.toml").to_str().unwrap()),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(fixture.global_path()).unwrap(),
        format!("[approved]\nguarded-dep = '{fingerprint}'\n")
    );
}

fn rejected_fingerprint(output: &Output) -> String {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find_map(|line| {
            line.split_once("Current fingerprint: ")
                .map(|(_, fingerprint)| fingerprint.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("missing fingerprint in build rejection:\n{stderr}"))
}

struct Fixture {
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new(approvals: &str) -> Self {
        let temp = tempfile::tempdir().expect("could not create e2e fixture directory");
        let root = temp.path().join("workspace");
        create_workspace(&root, Some(approvals));
        let binary = toml::Value::String(env!("CARGO_BIN_EXE_cargo-guard").into());
        write(
            &root,
            ".cargo/config.toml",
            &format!(
                r#"target-applies-to-host = false

[unstable]
host-config = true
target-applies-to-host = true

[host]
runner = [{binary}]

[env]
CARGO_GUARD_WORKSPACE_ROOT = {{ value = ".", relative = true }}
"#,
            ),
        );
        Self { temp }
    }

    fn global() -> Self {
        let temp = tempfile::tempdir().expect("could not create e2e fixture directory");
        for workspace in ["workspace", "workspace-b"] {
            let root = temp.path().join(workspace);
            create_workspace(&root, None);
            let script_path = root.join("dep/build.rs");
            let script = fs::read_to_string(&script_path).unwrap();
            fs::write(
                script_path,
                format!("{script}\nconst _: &str = include_str!(\"helper.txt\");\n"),
            )
            .unwrap();
            write(&root, "dep/helper.txt", "original");
        }
        let fixture = Self { temp };
        let binary = toml::Value::String(env!("CARGO_BIN_EXE_cargo-guard").into());
        let global = toml::Value::String(fixture.global_path().to_str().unwrap().into());
        write(
            fixture.temp.path(),
            "cargo-home/config.toml",
            &format!(
                r#"target-applies-to-host = false

[unstable]
host-config = true
target-applies-to-host = true

[host]
runner = [{binary}]

[env]
CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH = {global}
"#,
            ),
        );
        fixture
    }

    fn global_path(&self) -> PathBuf {
        self.temp.path().join("global/approved-builds.toml")
    }

    fn approve_global(&self, approvals: &str) {
        write(self.temp.path(), "global/approved-builds.toml", approvals);
    }

    fn workspace_marker(&self, workspace: &str) -> PathBuf {
        self.temp.path().join(format!("{workspace}-script-ran"))
    }

    fn build_workspace(&self, workspace: &str, configured_root: bool) -> Output {
        self.cargo(
            workspace,
            configured_root,
            &self.workspace_marker(workspace),
            &["build", "--offline", "--color", "never"],
        )
    }

    fn clean_dependency(&self, workspace: &str) {
        let output = self.cargo(
            workspace,
            true,
            &self.workspace_marker(workspace),
            &[
                "clean",
                "-p",
                "guarded-dep",
                "--offline",
                "--color",
                "never",
            ],
        );
        assert_success(&output);
    }

    fn marker(&self) -> PathBuf {
        self.temp.path().join("script-ran")
    }

    fn build(&self) -> Output {
        self.cargo(
            "workspace",
            false,
            &self.marker(),
            &["build", "--offline", "--color", "never"],
        )
    }

    fn cargo(
        &self,
        workspace: &str,
        configured_root: bool,
        marker: &Path,
        args: &[&str],
    ) -> Output {
        self.command(workspace, configured_root, marker)
            .arg("cargo")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| {
                panic!("could not run rustup run {TOOLCHAIN} cargo {args:?}: {error}")
            })
    }

    fn command(&self, workspace: &str, configured_root: bool, marker: &Path) -> Command {
        let root = self.temp.path();
        let cargo_home = root.join("cargo-home");
        fs::create_dir_all(&cargo_home)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", cargo_home.display()));
        let mut command = Command::new("rustup");
        command
            .args(["run", TOOLCHAIN])
            .current_dir(root.join(workspace));
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
        if configured_root {
            command.env("CARGO_GUARD_WORKSPACE_ROOT", root.join(workspace));
        }
        command
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", root.join(format!("target-{workspace}")))
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_GUARD_E2E_MARKER", marker);
        command
    }
}

#[derive(Default)]
struct TerminalResponses {
    cursor: Option<(u16, u16)>,
}

impl vt100::Callbacks for TerminalResponses {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        _intermediate: Option<u8>,
        _second_intermediate: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if command == 'n' && params == [&[6]] {
            self.cursor = Some(screen.cursor_position());
        }
    }
}

fn approve_review(command: &Command) {
    let mut builder = portable_pty::CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    builder.cwd(command.get_current_dir().unwrap());
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => builder.env(key, value),
            None => builder.env_remove(key),
        }
    }
    builder.env("TERM", "xterm-256color");
    let size = portable_pty::PtySize {
        rows: 40,
        cols: 160,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = portable_pty::native_pty_system().openpty(size).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let mut child = pair.slave.spawn_command(builder).unwrap();
    drop(pair.slave);
    let (sender, receiver) = mpsc::channel();
    let output = thread::spawn(move || {
        let mut buffer = [0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(Ok(buffer[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    let mut terminal =
        vt100::Parser::new_with_callbacks(size.rows, size.cols, 0, TerminalResponses::default());
    let mut accepted = false;
    let mut last_screen = String::new();
    let mut read_error = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = (|| -> eyre::Result<()> {
        loop {
            if let Some(status) = child.try_wait()? {
                eyre::ensure!(status.success(), "review exited with {status}");
                eyre::ensure!(accepted, "review exited before accepting a fingerprint");
                return Ok(());
            }
            eyre::ensure!(Instant::now() < deadline, "review timed out");
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(bytes)) => {
                    terminal.process(&bytes);
                    if let Some((row, column)) = terminal.callbacks_mut().cursor.take() {
                        write!(writer, "\x1b[{};{}R", row + 1, column + 1)?;
                        writer.flush()?;
                    }
                    let screen = terminal.screen().contents();
                    if !screen.trim().is_empty() {
                        last_screen = screen;
                    }
                    if !accepted
                        && last_screen.contains("guarded-dep v1.0.0")
                        && last_screen.contains("a accept")
                    {
                        writer.write_all(b"a")?;
                        writer.flush()?;
                        accepted = true;
                    }
                }
                Ok(Err(error)) => read_error = Some(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    })();
    if child.try_wait().unwrap().is_none() {
        child.kill().expect("could not stop failed review");
        child.wait().expect("could not reap failed review");
    }
    drop(writer);
    drop(pair.master);
    output.join().unwrap();
    assert!(
        result.is_ok(),
        "{result:?}\nterminal read error: {read_error:?}\n{last_screen}"
    );
}

fn create_workspace(root: &Path, approvals: Option<&str>) {
    write(
        root,
        "Cargo.toml",
        r#"[package]
name = "consumer"
version = "0.1.0"
edition = "2024"

[workspace]
exclude = ["dep"]

[dependencies]
guarded-dep = { path = "dep" }
"#,
    );
    write(
        root,
        "dep/Cargo.toml",
        r#"[package]
name = "guarded-dep"
version = "1.0.0"
edition = "2024"
build = "build.rs"

[workspace]
"#,
    );
    write(root, "src/lib.rs", "");
    write(root, "dep/src/lib.rs", "");
    if let Some(approvals) = approvals {
        write(root, "approved-builds.toml", approvals);
    }
    write(
        root,
        "dep/build.rs",
        r#"fn main() {
    std::fs::write(
        std::env::var_os("CARGO_GUARD_E2E_MARKER").expect("missing marker path"),
        b"executed\n",
    )
    .expect("could not write execution marker");
}
"#,
    );
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_rejected(output: &Output, marker: &Path, package: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unexpected success:\n{stderr}");
    assert!(
        stderr.contains(&format!("build script for {package}")),
        "missing Cargo Guard rejection:\n{stderr}"
    );
    assert!(
        !marker.try_exists().unwrap(),
        "rejected script executed:\n{stderr}"
    );
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("fixture file must have a parent"))
        .unwrap_or_else(|error| panic!("could not create parents for {}: {error}", path.display()));
    fs::write(&path, contents)
        .unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
}
