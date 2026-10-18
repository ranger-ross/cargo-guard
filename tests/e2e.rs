use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
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
        write(
            &root,
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
            &root,
            "dep/Cargo.toml",
            r#"[package]
name = "guarded-dep"
version = "1.0.0"
edition = "2024"
build = "build.rs"

[workspace]
"#,
        );
        write(&root, "src/lib.rs", "");
        write(&root, "dep/src/lib.rs", "");
        write(&root, "approved-builds.toml", approvals);
        write(
            &root,
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

    fn marker(&self) -> PathBuf {
        self.temp.path().join("script-ran")
    }

    fn build(&self) -> Output {
        let root = self.temp.path();
        let cargo_home = root.join("cargo-home");
        fs::create_dir_all(&cargo_home)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", cargo_home.display()));
        let mut command = Command::new("rustup");
        command
            .args([
                "run",
                TOOLCHAIN,
                "cargo",
                "build",
                "--offline",
                "--color",
                "never",
            ])
            .current_dir(root.join("workspace"))
            .stdin(Stdio::null());
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
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_GUARD_E2E_MARKER", self.marker())
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "could not run rustup run {TOOLCHAIN} cargo build --offline --color never in {}: {error}",
                    root.join("workspace").display()
                )
            })
    }
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("fixture file must have a parent"))
        .unwrap_or_else(|error| panic!("could not create parents for {}: {error}", path.display()));
    fs::write(&path, contents)
        .unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
}
