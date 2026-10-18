use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname='root'\nversion='1.0.0'\n[workspace]\nmembers=['member']\n",
    );
    write(root.path(), "src/lib.rs", "");
    write(
        root.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='1.0.0'\n",
    );
    write(root.path(), "member/src/lib.rs", "");
    root
}

#[test]
fn virtual_workspace_autoapproves_external_members() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "workspace/Cargo.toml",
        "[workspace]\nmembers=['../member']\nresolver='2'\n",
    );
    write(
        root.path(),
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='1.0.0'\nworkspace='../workspace'\n",
    );
    write(root.path(), "member/src/lib.rs", "");
    write(
        root.path(),
        "member/build.rs",
        r#"fn main() {
    std::fs::write(std::env::var_os("MEMBER_MARKER").unwrap(), b"executed").unwrap();
}
"#,
    );
    let binary = toml::Value::String(env!("CARGO_BIN_EXE_cargo-guard").into());
    write(
        root.path(),
        "cargo-home/config.toml",
        &format!(
            "target-applies-to-host=false\n[unstable]\nhost-config=true\ntarget-applies-to-host=true\n[host]\nrunner=[{binary}]\n"
        ),
    );
    let marker = root.path().join("member-ran");
    let directories = [root.path().join("workspace"), root.path().join("member")];
    #[cfg(unix)]
    let directories = {
        let mut directories = Vec::from(directories);
        let link = root.path().join("linked-member");
        std::os::unix::fs::symlink(root.path().join("member"), &link).unwrap();
        directories.push(link);
        directories
    };
    for (index, directory) in directories.iter().enumerate() {
        let mut command = Command::new("rustup");
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
        let output = command
            .args([
                "run",
                "nightly",
                "cargo",
                "build",
                "--offline",
                "--color",
                "never",
            ])
            .current_dir(directory)
            .env("CARGO_HOME", root.path().join("cargo-home"))
            .env(
                "CARGO_TARGET_DIR",
                root.path().join(format!("target-{index}")),
            )
            .env("MEMBER_MARKER", &marker)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(fs::read(&marker).unwrap(), b"executed");
        fs::remove_file(&marker).unwrap();
    }
    assert!(!root.path().join("workspace/approved-builds.toml").exists());
}

#[test]
fn explicit_workspace_review_needs_no_terminal_or_approval_file() {
    let root = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-guard"))
        .current_dir(root.path())
        .args(["review", "member"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!root.path().join("approved-builds.toml").exists());
}
