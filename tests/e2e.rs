use std::{
    fs,
    io::{Read, Write as _},
    num::NonZeroU16,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
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

#[test]
fn force_build_bypasses_approval_policy_without_changing_it() {
    for approvals in [
        Some("[approved]\n"),
        Some("[approved]\nguarded-dep = '2.0.0'\n"),
        Some("invalid toml ["),
        None,
    ] {
        let fixture = Fixture::new(approvals.unwrap_or(""));
        let path = fixture.temp.path().join("workspace/approved-builds.toml");
        if approvals.is_none() {
            fs::remove_file(&path).unwrap();
        }
        let output = fixture
            .command("workspace", &fixture.marker())
            .args(["cargo", "build", "--offline", "--color", "never"])
            .env("CARGO_GUARD_FORCE_BUILD", "force")
            .output()
            .unwrap();
        assert_success(&output);
        assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.lines().any(|line| {
                line.starts_with("warning:") && line.contains("CARGO_GUARD_FORCE_BUILD=force")
            }),
            "missing Cargo warning:\n{stderr}"
        );
        assert_eq!(fs::read_to_string(&path).ok().as_deref(), approvals);

        fixture.clean_dependency("workspace");
        fs::remove_file(fixture.marker()).unwrap();
        let rejected = fixture.build();
        assert!(!rejected.status.success(), "{rejected:?}");
        assert!(!fixture.marker().exists());
    }
}

#[test]
fn force_build_requires_exactly_force() {
    let fixture = Fixture::new("[approved]\n");
    for value in ["", "0", "1", "true", "Force", "force "] {
        let output = fixture
            .command("workspace", &fixture.marker())
            .args(["cargo", "build", "--offline", "--color", "never"])
            .env("CARGO_GUARD_FORCE_BUILD", value)
            .output()
            .unwrap();
        assert_rejected(&output, &fixture.marker(), "guarded-dep v1.0.0");
    }
}

#[test]
fn force_build_preserves_build_script_failure() {
    let fixture = Fixture::new("[approved]\n");
    write(
        &fixture.temp.path().join("workspace"),
        "dep/build.rs",
        "fn main() { std::process::exit(42); }\n",
    );
    let output = fixture
        .command("workspace", &fixture.marker())
        .args(["cargo", "build", "--offline", "--color", "never"])
        .env("CARGO_GUARD_FORCE_BUILD", "force")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let binary = fixture.temp.path().join(format!(
        "failing-build-script{}",
        std::env::consts::EXE_SUFFIX
    ));
    let compiled = fixture
        .command("workspace", &fixture.marker())
        .args(["rustc", "dep/build.rs", "-o"])
        .arg(&binary)
        .output()
        .unwrap();
    assert_success(&compiled);
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-guard"))
        .arg(&binary)
        .env("CARGO_GUARD_FORCE_BUILD", "force")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42), "{output:?}");
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

    assert_success(&fixture.build_workspace("workspace"));
    assert_eq!(fs::read(&marker).unwrap(), b"executed\n");
    fs::remove_file(&marker).unwrap();

    write(
        &root,
        "approved-builds.toml",
        "[approved]\nguarded-dep = '9.0.0'\n",
    );
    fixture.clean_dependency("workspace");
    let rejected = fixture.build_workspace("workspace");
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
    assert_success(&fixture.build_workspace("workspace"));
    assert_eq!(fs::read(&marker).unwrap(), b"executed\n");
    assert_eq!(
        fs::read_to_string(fixture.global_path()).unwrap(),
        "[approved]\nguarded-dep = '9.0.0'\n"
    );
}

#[test]
fn review_approval_pins_the_version_for_a_real_build() {
    let fixture = Fixture::new("[approved]\n");
    let root = fixture.temp.path().join("workspace");
    assert_rejected(&fixture.build(), &fixture.marker(), "guarded-dep v1.0.0");
    let mut review = fixture.command("workspace", &fixture.marker());
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
    assert!(saved.contains("guarded-dep = \"1.0.0\""), "{saved}");

    assert_success(&fixture.build());
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
    fs::remove_file(fixture.marker()).unwrap();

    let manifest = fs::read_to_string(root.join("dep/Cargo.toml")).unwrap();
    write(
        &root,
        "dep/Cargo.toml",
        &manifest.replace("version = \"1.0.0\"", "version = \"1.0.1\""),
    );
    assert_rejected(&fixture.build(), &fixture.marker(), "guarded-dep v1.0.1");
    assert_eq!(fs::read_to_string(&policy_path).unwrap(), saved);
}

#[test]
fn global_version_approval_authorizes_two_workspaces() {
    let fixture = Fixture::global();
    for workspace in ["workspace", "workspace-b"] {
        let rejected = fixture.build_workspace(workspace);
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
        assert_success(&fixture.build_workspace(workspace));
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
fn global_build_autoapproves_workspace_member_scripts() {
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
    assert_success(&fixture.build_workspace("workspace"));
    assert_eq!(fs::read(member_marker).unwrap(), b"executed\n");
    assert!(!root.join("approved-builds.toml").exists());
}

#[test]
fn manifest_path_uses_invoking_workspace_from_dependency_and_unrelated_directories() {
    let fixture = Fixture::global();
    fs::create_dir(fixture.temp.path().join("unrelated")).unwrap();
    for (directory, manifest) in [
        ("workspace/dep", "../Cargo.toml"),
        ("unrelated", "../workspace/Cargo.toml"),
    ] {
        let output = fixture
            .command("workspace", &fixture.marker())
            .current_dir(fixture.temp.path().join(directory))
            .args([
                "cargo",
                "build",
                "--offline",
                "--color",
                "never",
                "--manifest-path",
                manifest,
            ])
            .output()
            .unwrap();
        assert_rejected(&output, &fixture.marker(), "guarded-dep v1.0.0");
    }
    fixture.approve_global("[approved]\nguarded-dep = '1.0.0'\n");
    let output = fixture
        .command("workspace", &fixture.marker())
        .current_dir(fixture.temp.path().join("unrelated"))
        .args([
            "cargo",
            "build",
            "--offline",
            "--manifest-path=../workspace/Cargo.toml",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
}

#[test]
fn member_directory_build_autoapproves_members_but_not_excluded_dependencies() {
    let fixture = Fixture::global();
    let root = fixture.temp.path().join("workspace");
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    write(
        &root,
        "Cargo.toml",
        &manifest.replace("[workspace]", "[workspace]\nmembers = ['member']"),
    );
    write(
        &root,
        "member/Cargo.toml",
        "[package]\nname='member'\nversion='1.0.0'\nedition='2024'\n[dependencies]\nguarded-dep={path='../dep'}\n",
    );
    write(&root, "member/src/lib.rs", "");
    write(
        &root,
        "member/build.rs",
        r#"fn main() {
    let marker = std::path::PathBuf::from(std::env::var_os("CARGO_GUARD_E2E_MARKER").unwrap());
    std::fs::write(marker.with_extension("member"), b"executed\n").unwrap();
}
"#,
    );
    let build = || {
        fixture
            .command("workspace", &fixture.marker())
            .current_dir(root.join("member"))
            .args(["cargo", "build", "--offline", "--color", "never"])
            .output()
            .unwrap()
    };
    assert_rejected(&build(), &fixture.marker(), "guarded-dep v1.0.0");
    fixture.approve_global("[approved]\nguarded-dep = '1.0.0'\n");
    assert_success(&build());
    assert_eq!(
        fs::read(fixture.marker().with_extension("member")).unwrap(),
        b"executed\n"
    );
    assert!(!root.join("approved-builds.toml").exists());
}

#[test]
fn builtin_and_recursive_project_aliases_preserve_workspace_policy() {
    for alias in ["b", "c", "guarded-build", "guarded-chain", "local-b"] {
        let fixture = Fixture::global();
        let root = fixture.temp.path().join("workspace");
        fixture.approve_global("[approved]\nguarded-dep = 'approved'\n");
        write(&root, "approved-builds.toml", "[approved]\n");
        let config_path = fixture.temp.path().join("cargo-home/config.toml");
        let config = fs::read_to_string(&config_path).unwrap();
        fs::write(
            config_path,
            format!("{config}\n[alias]\nguarded-build = 'build --manifest-path dep/Cargo.toml'\n"),
        )
        .unwrap();
        write(
            &root,
            ".cargo/config.toml",
            "[alias]\nguarded-build = 'build --offline'\nguarded-chain = 'guarded-build'\n",
        );
        let command = if alias == "local-b" {
            write(
                &root,
                ".cargo/config.toml",
                "[alias]\nb = 'check --offline'\n",
            );
            "b"
        } else {
            alias
        };
        let build = || {
            fixture.cargo(
                "workspace",
                &fixture.marker(),
                &[command, "--color", "never"],
            )
        };
        let rejected = build();
        assert_rejected(&rejected, &fixture.marker(), "guarded-dep v1.0.0");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("no approval entry"),
            "{rejected:?}"
        );
        write(
            &root,
            "approved-builds.toml",
            "[approved]\nguarded-dep = '1.0.0'\n",
        );
        assert_success(&build());
        assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
    }
}

#[test]
fn array_alias_manifest_with_spaces_ignores_late_cli_alias_override() {
    let fixture = Fixture::global();
    let root = fixture.temp.path().join("workspace with spaces");
    fs::rename(fixture.temp.path().join("workspace"), &root).unwrap();
    fixture.approve_global("[approved]\nguarded-dep = 'approved'\n");
    write(&root, "approved-builds.toml", "[approved]\n");
    let config_path = fixture.temp.path().join("cargo-home/config.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    let manifest = toml::Value::String(root.join("Cargo.toml").to_str().unwrap().into());
    fs::write(
        config_path,
        format!("{config}\n[alias]\nguarded-build = ['build', '--offline', '--manifest-path', {manifest}]\n"),
    )
    .unwrap();
    let build = || {
        fixture
            .command("workspace with spaces", &fixture.marker())
            .current_dir(root.join("dep"))
            .args([
                "cargo",
                "--config",
                "alias.guarded-build=['build','--offline','--manifest-path','Cargo.toml']",
                "guarded-build",
            ])
            .output()
            .unwrap()
    };
    let rejected = build();
    assert_rejected(&rejected, &fixture.marker(), "guarded-dep v1.0.0");
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("no approval entry"), "{stderr}");
    assert!(
        stderr.contains(root.join("approved-builds.toml").to_str().unwrap()),
        "{stderr}"
    );
    write(
        &root,
        "approved-builds.toml",
        "[approved]\nguarded-dep = '1.0.0'\n",
    );
    assert_success(&build());
    assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
}

#[test]
fn environment_aliases_use_invoking_cargo_environment_and_merge_rules() {
    for source in [
        "environment-only",
        "string",
        "array",
        "forced-runner-env",
        "advanced-array",
    ] {
        let fixture = Fixture::global();
        let root = fixture.temp.path().join("workspace");
        fixture.approve_global("[approved]\nguarded-dep = 'approved'\n");
        write(&root, "approved-builds.toml", "[approved]\n");
        let config_path = fixture.temp.path().join("cargo-home/config.toml");
        let mut config = fs::read_to_string(&config_path).unwrap();
        match source {
            "string" => config.push_str(
                "\n[alias]\nguarded-build = 'build --offline --manifest-path Cargo.toml'\n",
            ),
            "array" | "advanced-array" => {
                config.push_str("\n[alias]\nguarded-build = ['build', '--offline']\n");
                if source == "advanced-array" {
                    config = config.replace(
                        "host-config = true",
                        "host-config = true\nadvanced-env = true",
                    );
                }
            }
            "forced-runner-env" => {
                // This is in the existing [env] table. Cargo expands its alias
                // before applying this value to the build-script runner.
                config.push_str(
                    "CARGO_ALIAS_GUARDED_BUILD = { value = 'build --offline --manifest-path Cargo.toml', force = true }\n",
                );
            }
            _ => {}
        }
        fs::write(config_path, config).unwrap();
        let expansion = if source == "advanced-array" {
            "['--manifest-path', '../Cargo.toml']"
        } else if source == "array" {
            "--manifest-path ../Cargo.toml"
        } else {
            "build --offline --manifest-path ../Cargo.toml"
        };
        let build = || {
            let mut command = fixture.command("workspace", &fixture.marker());
            command
                .current_dir(root.join("dep"))
                .env("CARGO_ALIAS_GUARDED_BUILD", expansion)
                .arg("cargo");
            if source == "advanced-array" {
                command
                    .args(["-Zunstable-options", "-C"])
                    .arg(root.join("dep"));
            }
            command.arg("guarded-build").output().unwrap()
        };
        let rejected = build();
        assert_rejected(&rejected, &fixture.marker(), "guarded-dep v1.0.0");
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(stderr.contains("no approval entry"), "{source}: {stderr}");
        assert!(
            stderr.contains(root.join("approved-builds.toml").to_str().unwrap()),
            "{source}: {stderr}"
        );
        write(
            &root,
            "approved-builds.toml",
            "[approved]\nguarded-dep = '1.0.0'\n",
        );
        assert_success(&build());
        assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
    }
}

#[test]
fn shared_custom_target_directory_preserves_discovery_on_cached_script_reruns() {
    let fixture = Fixture::global();
    fixture.approve_global("[approved]\nguarded-dep = '1.0.0'\n");
    let target = fixture.temp.path().join("shared-artifacts");
    for workspace in ["workspace", "workspace-b"] {
        let root = fixture.temp.path().join(workspace);
        let script = fs::read_to_string(root.join("dep/build.rs")).unwrap();
        write(
            &root,
            "dep/build.rs",
            &script.replace(
                "fn main() {",
                "fn main() {\n    println!(\"cargo::rerun-if-env-changed=CARGO_GUARD_E2E_RERUN\");",
            ),
        );
        let build = |rerun: &str| {
            fixture
                .command(workspace, &fixture.workspace_marker(workspace))
                .args(["cargo", "build", "--offline", "--target-dir"])
                .arg(&target)
                .env("CARGO_GUARD_E2E_RERUN", rerun)
                .output()
                .unwrap()
        };
        assert_success(&build("initial"));
        assert_eq!(
            fs::read(fixture.workspace_marker(workspace)).unwrap(),
            b"executed\n"
        );
        fs::remove_file(fixture.workspace_marker(workspace)).unwrap();
        write(&root, "approved-builds.toml", "[approved]\n");
        // Only an observed environment input changes: Cargo reuses the compiled
        // build script, so discovery cannot depend on a rustc invocation.
        assert_rejected(
            &build("changed"),
            &fixture.workspace_marker(workspace),
            "guarded-dep v1.0.0",
        );
    }
}

#[test]
fn nested_cargo_keeps_outer_workspace_policy_authoritative() {
    let fixture = Fixture::global();
    let root = fixture.temp.path().join("workspace");
    let inner = fixture.temp.path().join("inner");
    fixture.approve_global("[approved]\nguarded-dep='1.0.0'\ninner='1.0.0'\n");
    write(
        &root,
        "approved-builds.toml",
        "[approved]\nguarded-dep='1.0.0'\n",
    );
    write(
        &inner,
        "Cargo.toml",
        "[package]\nname='inner'\nversion='1.0.0'\nedition='2024'\n[workspace]\n",
    );
    write(&inner, "src/lib.rs", "");
    write(
        &inner,
        "approved-builds.toml",
        "[approved]\ninner='1.0.0'\n",
    );
    write(
        &inner,
        "build.rs",
        r#"fn main() {
    let marker = std::path::PathBuf::from(std::env::var_os("CARGO_GUARD_E2E_MARKER").unwrap());
    std::fs::write(marker.with_extension("inner"), b"executed\n").unwrap();
}
"#,
    );
    write(
        &root,
        "dep/build.rs",
        r#"fn main() {
    let inner = std::path::PathBuf::from(std::env::var_os("CARGO_GUARD_E2E_INNER").unwrap());
    let status = std::process::Command::new(std::env::var_os("CARGO").unwrap())
        .args(["build", "--offline", "--color", "never", "--manifest-path"])
        .arg(inner.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(inner.join("artifacts"))
        .current_dir(&inner)
        .status()
        .unwrap();
    assert!(status.success(), "nested Cargo build was rejected");
}
"#,
    );
    let build = || {
        fixture
            .command("workspace", &fixture.marker())
            .args(["cargo", "build", "--offline", "--color", "never"])
            .env("CARGO_GUARD_E2E_INNER", &inner)
            .output()
            .unwrap()
    };
    let rejected = build();
    assert_rejected(
        &rejected,
        &fixture.marker().with_extension("inner"),
        "inner v1.0.0",
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains(root.join("approved-builds.toml").to_str().unwrap()),
        "{rejected:?}"
    );
    write(
        &root,
        "approved-builds.toml",
        "[approved]\nguarded-dep='1.0.0'\ninner='1.0.0'\n",
    );
    assert_success(&build());
    assert_eq!(
        fs::read(fixture.marker().with_extension("inner")).unwrap(),
        b"executed\n"
    );
}

#[test]
fn version_list_approvals_authorize_only_matching_dependency_versions() {
    for (approvals, allowed) in [
        ("['0.9.0', '1.0.0']", true),
        ("[]", false),
        ("['0.9.0', '0.8.0']", false),
    ] {
        let fixture = Fixture::new(&format!("[approved]\nguarded-dep={approvals}\n"));
        let output = fixture.build();
        if allowed {
            assert_success(&output);
            assert_eq!(fs::read(fixture.marker()).unwrap(), b"executed\n");
        } else {
            assert_rejected(&output, &fixture.marker(), "guarded-dep v1.0.0");
        }
    }
}

#[test]
fn invalid_workspace_approval_files_never_fall_back_to_global_policy() {
    for contents in [
        "[approved]\n",
        "[approved]\nguarded-dep='2.0.0'\n",
        "invalid toml",
        "unrelated=true",
    ] {
        let fixture = Fixture::global();
        fixture.approve_global("[approved]\nguarded-dep='1.0.0'\n");
        write(
            &fixture.temp.path().join("workspace"),
            "approved-builds.toml",
            contents,
        );
        let output = fixture.build_workspace("workspace");
        assert!(!output.status.success(), "{output:?}");
        assert!(
            !fixture.workspace_marker("workspace").exists(),
            "{output:?}"
        );
    }
    let fixture = Fixture::global();
    fixture.approve_global("[approved]\nguarded-dep='1.0.0'\n");
    let policy = fixture.temp.path().join("workspace/approved-builds.toml");
    fs::create_dir(&policy).unwrap();
    let output = fixture.build_workspace("workspace");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        !fixture.workspace_marker("workspace").exists(),
        "{output:?}"
    );
    #[cfg(unix)]
    {
        fs::remove_dir(&policy).unwrap();
        std::os::unix::fs::symlink(fixture.temp.path().join("missing-policy"), &policy).unwrap();
        let output = fixture.build_workspace("workspace");
        assert!(!output.status.success(), "{output:?}");
        assert!(
            !fixture.workspace_marker("workspace").exists(),
            "{output:?}"
        );
    }
}

#[test]
fn workspace_approval_does_not_read_invalid_global_file() {
    let fixture = Fixture::global();
    fixture.approve_global("malformed");
    write(
        &fixture.temp.path().join("workspace"),
        "approved-builds.toml",
        "[approved]\nguarded-dep='1.0.0'\n",
    );
    assert_success(&fixture.build_workspace("workspace"));
    assert_eq!(
        fs::read(fixture.workspace_marker("workspace")).unwrap(),
        b"executed\n"
    );
}

#[test]
fn invalid_global_path_is_rejected_even_with_workspace_approvals() {
    let fixture = Fixture::new("[approved]\nguarded-dep='1.0.0'\n");
    for invalid in ["", "relative", "~/approvals.toml"] {
        let output = fixture
            .command("workspace", &fixture.marker())
            .args(["cargo", "build", "--offline", "--color", "never"])
            .env("CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH", invalid)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH"),
            "{output:?}"
        );
        assert!(!fixture.marker().exists(), "{output:?}");
    }
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

    fn build_workspace(&self, workspace: &str) -> Output {
        self.cargo(
            workspace,
            &self.workspace_marker(workspace),
            &["build", "--offline", "--color", "never"],
        )
    }

    fn clean_dependency(&self, workspace: &str) {
        let output = self.cargo(
            workspace,
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
            &self.marker(),
            &["build", "--offline", "--color", "never"],
        )
    }

    fn cargo(&self, workspace: &str, marker: &Path, args: &[&str]) -> Output {
        self.command(workspace, marker)
            .arg("cargo")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| {
                panic!("could not run rustup run {TOOLCHAIN} cargo {args:?}: {error}")
            })
    }

    fn command(&self, workspace: &str, marker: &Path) -> Command {
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
    let mut terminal = vt100::Parser::new_with_callbacks(
        NonZeroU16::new(size.rows).unwrap(),
        NonZeroU16::new(size.cols).unwrap(),
        0,
        TerminalResponses::default(),
    );
    let mut accepted = false;
    let mut last_screen = String::new();
    let mut read_error = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = (|| -> eyre::Result<()> {
        loop {
            if let Some(status) = child.try_wait()? {
                eyre::ensure!(status.success(), "review exited with {status}");
                eyre::ensure!(accepted, "review exited before approving");
                return Ok(());
            }
            eyre::ensure!(Instant::now() < deadline, "review timed out");
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(bytes)) => {
                    terminal.process(&bytes);
                    if let Some((row, column)) = terminal.callbacks_mut().cursor.take() {
                        // ConPTY's startup reply must not be split across writes.
                        let response = format!("\x1b[{};{}R", row + 1, column + 1);
                        writer.write_all(response.as_bytes())?;
                        writer.flush()?;
                    }
                    let screen = terminal.screen().contents();
                    if !screen.trim().is_empty() {
                        last_screen = screen;
                    }
                    if !accepted
                        && last_screen.contains("guarded-dep v1.0.0")
                        && last_screen.contains("a approve")
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
