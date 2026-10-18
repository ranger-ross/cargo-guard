#![cfg(unix)]

use std::{fs, path::Path, process::Command};

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
        "[package]\nname='root'\nversion='1.0.0'\n[workspace]\nmembers=['member']\nexclude=['excluded']\n",
    );
    write(root.path(), "src/lib.rs", "");
    for name in ["member", "excluded"] {
        write(
            root.path(),
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname='{name}'\nversion='1.0.0'\n"),
        );
        write(root.path(), &format!("{name}/src/lib.rs"), "");
    }
    root
}

fn runner(root: &Path, manifest: &Path, name: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-guard"));
    command
        .env_remove("CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH")
        .env("CARGO_GUARD_WORKSPACE_ROOT", root)
        .env("CARGO_MANIFEST_PATH", manifest)
        .env("CARGO_PKG_NAME", name)
        .env("CARGO_PKG_VERSION", "1.0.0")
        .env("OUT_DIR", root.join("out"));
    command
}

#[test]
fn workspace_scripts_bypass_approvals_and_forward_arguments_and_status() {
    let root = fixture();
    for manifest in ["Cargo.toml", "member/Cargo.toml"] {
        let output = runner(root.path(), &root.path().join(manifest), "member")
            .args([
                "/bin/sh",
                "-c",
                "printf '%s' \"$1\"; exit 23",
                "script",
                "forwarded argument",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(23), "{:?}", output);
        assert_eq!(output.stdout, b"forwarded argument");
    }
    assert!(!root.path().join("approved-builds.toml").exists());
}

#[test]
fn excluded_packages_still_require_approval() {
    let root = fixture();
    write(root.path(), "approved-builds.toml", "[approved]\n");
    let output = runner(
        root.path(),
        &root.path().join("excluded/Cargo.toml"),
        "excluded",
    )
    .args(["/bin/sh", "-c", "echo should-not-run"])
    .output()
    .unwrap();
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("no approval entry"));
    assert!(!text.contains("should-not-run"));
    write(
        root.path(),
        "approved-builds.toml",
        "[approved]\nexcluded='1.0.0'\n",
    );
    let output = runner(
        root.path(),
        &root.path().join("excluded/Cargo.toml"),
        "excluded",
    )
    .args(["/bin/sh", "-c", "echo approved-script"])
    .output()
    .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"approved-script\n");
}

#[test]
fn version_lists_match_without_reading_fingerprint_inputs() {
    let root = fixture();
    for approvals in ["['0.9.0','1.0.0']", "['wrong-fingerprint','1.0.0']"] {
        write(
            root.path(),
            "approved-builds.toml",
            &format!("[approved]\nexcluded={approvals}\n"),
        );
        let output = runner(
            root.path(),
            &root.path().join("excluded/Cargo.toml"),
            "excluded",
        )
        .args(["/bin/sh", "-c", "true"])
        .output()
        .unwrap();
        assert!(output.status.success(), "{:?}", output);
    }
    for approvals in ["[]", "['0.9.0','0.8.0']"] {
        write(
            root.path(),
            "approved-builds.toml",
            &format!("[approved]\nexcluded={approvals}\n"),
        );
        let output = runner(
            root.path(),
            &root.path().join("excluded/Cargo.toml"),
            "excluded",
        )
        .args(["/bin/sh", "-c", "true"])
        .output()
        .unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn virtual_workspace_allows_external_members_and_manifest_symlinks() {
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
    let workspace = root.path().join("workspace");
    let manifest = root.path().join("member/Cargo.toml");
    let output = runner(&workspace, &manifest, "member")
        .args(["/bin/sh", "-c", "true"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    #[cfg(unix)]
    {
        let link = root.path().join("linked-manifest");
        std::os::unix::fs::symlink(&manifest, &link).unwrap();
        let output = runner(&workspace, &link, "member")
            .args(["/bin/sh", "-c", "true"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
    }
}

#[test]
fn explicit_workspace_review_needs_no_terminal_or_approval_file() {
    let root = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-guard"))
        .current_dir(root.path())
        .args(["review", "member"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("do not require approval"));
}

#[test]
fn runner_selects_one_store_and_requires_explicit_activation() {
    let root = fixture();
    let global = root.path().join("personal.toml");
    fs::write(
        &global,
        "[approved]\nexcluded=['unreadable-fingerprint','1.0.0']\nmember='1.0.0'\n",
    )
    .unwrap();
    let run = |workspace: bool, global_enabled: bool, package: &str| {
        let mut command = runner(
            root.path(),
            &root.path().join(format!("{package}/Cargo.toml")),
            package,
        );
        if !workspace {
            command.env_remove("CARGO_GUARD_WORKSPACE_ROOT");
        }
        if global_enabled {
            command.env("CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH", &global);
        }
        command
            .args(["/bin/sh", "-c", "echo executed"])
            .output()
            .unwrap()
    };
    assert!(!run(false, false, "excluded").status.success());
    assert!(run(false, true, "excluded").status.success());
    assert!(run(false, true, "member").status.success());
    fs::write(&global, "[approved]\nexcluded='1.0.0'\n").unwrap();
    let rejected = run(false, true, "member");
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stdout).contains("executed"));
    assert!(run(true, true, "excluded").status.success());
    let workspace = root.path().join("approved-builds.toml");
    for contents in [
        "[approved]\n",
        "[approved]\nexcluded='2.0.0'\n",
        "invalid toml",
        "unrelated=true",
    ] {
        fs::write(&workspace, contents).unwrap();
        let rejected = run(true, true, "excluded");
        assert!(!rejected.status.success(), "{rejected:?}");
        assert!(!String::from_utf8_lossy(&rejected.stdout).contains("executed"));
    }
    fs::remove_file(&workspace).unwrap();
    std::os::unix::fs::symlink(root.path().join("missing"), &workspace).unwrap();
    assert!(!run(true, true, "excluded").status.success());
    fs::remove_file(&workspace).unwrap();
    fs::create_dir(&workspace).unwrap();
    assert!(!run(true, true, "excluded").status.success());
    fs::remove_dir(&workspace).unwrap();
    fs::write(&workspace, "[approved]\nexcluded='1.0.0'\n").unwrap();
    fs::write(&global, "malformed").unwrap();
    assert!(run(true, true, "excluded").status.success());
    fs::remove_file(&global).unwrap();
    assert!(!run(false, true, "excluded").status.success());
}

#[test]
fn runner_validates_even_unselected_paths() {
    let root = fixture();
    for variable in [
        "CARGO_GUARD_WORKSPACE_ROOT",
        "CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH",
    ] {
        for invalid in ["", "relative", "~/approvals.toml"] {
            let output = runner(root.path(), &root.path().join("Cargo.toml"), "root")
                .env(variable, invalid)
                .args(["/bin/sh", "-c", "echo executed"])
                .output()
                .unwrap();
            assert!(!output.status.success());
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains(variable), "{text}");
            assert!(!text.contains("executed"));
        }
    }
}
