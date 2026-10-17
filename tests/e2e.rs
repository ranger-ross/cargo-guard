use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

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
