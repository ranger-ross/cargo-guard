use std::{
    collections::BTreeSet,
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

fn hashes(output: &Output) -> BTreeSet<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| {
            line.split_once("Current fingerprint: ")
                .map(|(_, hash)| hash.trim().to_owned())
        })
        .collect()
}

#[test]
#[ignore = "requires nightly Cargo multiple-build-scripts and host runner support"]
fn real_multiple_scripts_have_independent_fingerprints() {
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
    let mut approved = BTreeSet::from(["not-approved".to_owned()]);
    for _ in 0..3 {
        write(
            root,
            "approved-builds.toml",
            &format!(
                "[approved]\nmulti={:?}\n",
                approved.iter().collect::<Vec<_>>()
            ),
        );
        let output = build(root);
        if output.status.success() {
            break;
        }
        let found = hashes(&output);
        assert!(
            !found.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        approved.extend(found);
    }
    assert_eq!(approved.len(), 3);
    assert!(
        Command::new("cargo")
            .arg("clean")
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = build(root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in ["first ran", "second ran"] {
        assert!(String::from_utf8_lossy(&output.stderr).contains(name));
    }

    write(
        root,
        "dep/first.rs",
        "fn main() { println!(\"cargo::warning=first changed\"); }",
    );
    assert!(
        Command::new("cargo")
            .arg("clean")
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = build(root);
    assert!(!output.status.success());
    let changed = hashes(&output);
    assert_eq!(
        changed.len(),
        1,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(changed.is_disjoint(&approved));
    assert!(String::from_utf8_lossy(&output.stderr).contains("second ran"));

    write(
        root,
        "approved-builds.toml",
        "[approved]\nmulti=['0.9.0','1.0.0','wrong-hash']\n",
    );
    let output = build(root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
