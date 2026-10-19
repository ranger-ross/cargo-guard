use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn build(root: &Path) -> Output {
    Command::new("cargo")
        .args(["build", "--offline"])
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
#[ignore = "requires nightly Cargo multiple-build-scripts and host runner support"]
fn real_multiple_scripts_share_one_version_approval() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname='consumer'\nversion='0.1.0'\nedition='2024'\n[workspace]\nexclude=['dep']\n[dependencies]\nmulti={path='dep'}\n",
    );
    write(root, "src/lib.rs", "");
    write(root, "dep/src/lib.rs", "");
    write(
        root,
        "dep/Cargo.toml",
        "cargo-features=['multiple-build-scripts']\n[package]\nname='multi'\nversion='1.0.0'\nedition='2024'\nbuild=['first.rs','second.rs']\n[workspace]\n",
    );
    for name in ["first", "second"] {
        write(
            root,
            &format!("dep/{name}.rs"),
            &format!("fn main() {{ println!(\"cargo::warning={name} ran\"); }}"),
        );
    }
    let binary = toml::Value::String(env!("CARGO_BIN_EXE_cargo-guard").into());
    write(
        root,
        ".cargo/config.toml",
        &format!(
            "[unstable]\nhost-config=true\ntarget-applies-to-host=true\n[host]\nrunner={binary}\n"
        ),
    );

    write(root, "approved-builds.toml", "[approved]\nmulti='0.9.0'\n");
    let output = build(root);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("build script for multi v1.0.0 is not approved"),
        "{stderr}"
    );
    for name in ["first ran", "second ran"] {
        assert!(!stderr.contains(name), "{stderr}");
    }

    write(
        root,
        "approved-builds.toml",
        "[approved]\nmulti=['0.9.0','1.0.0']\n",
    );
    let output = build(root);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    for name in ["first ran", "second ran"] {
        assert!(stderr.contains(name), "{stderr}");
    }
}
