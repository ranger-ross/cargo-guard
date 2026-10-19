use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use cargo_metadata::Package;
use eyre::ContextCompat;
use serde::Deserialize;
use url::Url;

#[derive(Clone, Debug, Default)]
pub struct BrowserSource {
    base: Option<Url>,
}

impl BrowserSource {
    pub fn from_package(package: &Package) -> Self {
        let Some(source) = package.source.as_ref().map(|source| source.repr.as_str()) else {
            return Self::default();
        };
        let github = if source.starts_with("registry+") || source.starts_with("sparse+") {
            registry_github_source(package)
        } else if let Some(source) = source.strip_prefix("git+") {
            git_github_source(package, source)
        } else {
            None
        };
        let base = github.or_else(|| {
            is_crates_io(source).then(|| {
                let mut url = Url::parse("https://docs.rs/").expect("static docs.rs URL");
                url.path_segments_mut()
                    .expect("HTTPS URL supports path segments")
                    .extend([
                        "crate",
                        package.name.as_str(),
                        &package.version.to_string(),
                        "source",
                    ]);
                url
            })
        });
        Self { base }
    }

    pub fn url(&self, path: &Path) -> eyre::Result<String> {
        let path = path
            .to_str()
            .context("cannot open a browser URL for a non-UTF-8 script path")?;
        let separator = |character| character == '/' || (cfg!(windows) && character == '\\');
        eyre::ensure!(
            path.split(separator)
                .all(|segment| relative_path(segment, false)),
            "browser script path must remain package-relative"
        );
        let mut url = self.base.clone().context(
            "browser source unavailable: no usable pinned GitHub provenance or crates.io release",
        )?;
        url.path_segments_mut()
            .expect("HTTPS URL supports path segments")
            .extend(path.split(separator));
        Ok(url.into())
    }
}

#[derive(Deserialize)]
struct VcsInfo {
    git: VcsGit,
    #[serde(default)]
    path_in_vcs: String,
}

#[derive(Deserialize)]
struct VcsGit {
    sha1: String,
    #[serde(default)]
    dirty: bool,
}

fn registry_github_source(package: &Package) -> Option<Url> {
    let repository = github_repository(package.repository.as_deref()?)?;
    let root = package.manifest_path.parent()?;
    let bytes = fs::read(root.join(".cargo_vcs_info.json")).ok()?;
    let provenance: VcsInfo = serde_json::from_slice(&bytes).ok()?;
    if provenance.git.dirty
        || !full_revision(&provenance.git.sha1)
        || !relative_path(&provenance.path_in_vcs, true)
    {
        return None;
    }
    // Publisher metadata provides a convenience link, not proof of code equivalence.
    Some(github_script_root(
        repository,
        &provenance.git.sha1,
        &provenance.path_in_vcs,
    ))
}

fn git_github_source(package: &Package, source: &str) -> Option<Url> {
    let (repository, revision) = source.rsplit_once('#')?;
    if !full_revision(revision) {
        return None;
    }
    let repository = if let Some((repository, query)) = repository.split_once('?') {
        let mut pairs = url::form_urlencoded::parse(query.as_bytes());
        let (key, _) = pairs.next()?;
        if !matches!(key.as_ref(), "branch" | "tag" | "rev") || pairs.next().is_some() {
            return None;
        }
        repository
    } else {
        repository
    };
    let repository = github_repository(repository)?;
    let root = package.manifest_path.parent()?;
    // rev-parse only reads the checkout: no fetching, hooks, or source execution.
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD", "--show-prefix"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let output = std::str::from_utf8(&output.stdout).ok()?;
    let (head, prefix) = output.split_once('\n')?;
    if !head.eq_ignore_ascii_case(revision) {
        return None;
    }
    let prefix = prefix.strip_suffix('\n')?;
    let package_path = prefix.strip_suffix('/').unwrap_or(prefix);
    if !relative_path(package_path, true) {
        return None;
    }
    Some(github_script_root(repository, revision, package_path))
}

fn github_script_root(mut repository: Url, revision: &str, package_path: &str) -> Url {
    let mut segments = repository
        .path_segments_mut()
        .expect("HTTPS URL supports path segments");
    segments.extend(["blob", revision]);
    if !package_path.is_empty() {
        segments.extend(package_path.split('/'));
    }
    drop(segments);
    repository
}

fn is_crates_io(source: &str) -> bool {
    matches!(
        source,
        "registry+https://github.com/rust-lang/crates.io-index"
            | "sparse+https://index.crates.io/"
            | "registry+sparse+https://index.crates.io/"
    )
}

fn full_revision(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn relative_path(path: &str, allow_empty: bool) -> bool {
    if path.is_empty() {
        return allow_empty;
    }
    if path.contains('\\')
        || path.bytes().any(|byte| byte.is_ascii_control())
        || (path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic())
    {
        return false;
    }
    path.split('/')
        .all(|segment| !matches!(segment, "" | "." | ".."))
}

fn github_repository(repository: &str) -> Option<Url> {
    // Validate the original spelling before URL parsing can normalize traversal.
    let (authority, path, ssh) = if let Some((scheme, rest)) = repository.split_once("://") {
        if !matches!(scheme, "http" | "https" | "git" | "ssh") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        (authority, path, scheme == "ssh")
    } else {
        let (authority, path) = repository.split_once(':')?;
        (authority, path, true)
    };
    let host = if ssh {
        authority.strip_prefix("git@").unwrap_or(authority)
    } else {
        authority
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    let (owner, repository) = path.split_once('/')?;
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if owner.is_empty()
        || owner.starts_with('-')
        || owner.ends_with('-')
        || !owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || matches!(repository, "" | "." | "..")
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    let mut url = Url::parse("https://github.com/").expect("static GitHub URL");
    url.path_segments_mut()
        .expect("HTTPS URL supports path segments")
        .extend([owner, repository]);
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

    fn package(root: &Path, source: Option<&str>, repository: Option<&str>) -> Package {
        serde_json::from_value(serde_json::json!({
            "name": "example",
            "version": "1.2.3-beta.4+build.5",
            "id": "example 1.2.3-beta.4+build.5",
            "source": source,
            "repository": repository,
            "manifest_path": root.join("Cargo.toml"),
            "dependencies": [],
            "targets": [],
            "features": {},
        }))
        .unwrap()
    }

    fn provenance(root: &Path, path: &str) {
        fs::write(
            root.join(".cargo_vcs_info.json"),
            serde_json::json!({"git": {"sha1": REVISION}, "path_in_vcs": path}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn registry_root_and_monorepo_use_pinned_custom_script_paths() {
        let root = tempfile::tempdir().unwrap();
        let package = package(
            root.path(),
            Some(CRATES_IO),
            Some("https://github.com/org/repo.git"),
        );
        for prefix in ["", "crates/example", "crates/space % #"] {
            provenance(root.path(), prefix);
            let source = BrowserSource::from_package(&package);
            let encoded_prefix = match prefix {
                "" => "",
                "crates/example" => "crates/example/",
                _ => "crates/space%20%25%20%23/",
            };
            assert_eq!(
                source.url(Path::new("scripts/custom % #.rs")).unwrap(),
                format!(
                    "https://github.com/org/repo/blob/{REVISION}/{encoded_prefix}scripts/custom%20%25%20%23.rs"
                )
            );
        }
    }

    #[test]
    fn captures_provenance_before_sources_are_removed() {
        let root = tempfile::tempdir().unwrap();
        provenance(root.path(), "nested");
        let source = BrowserSource::from_package(&package(
            root.path(),
            Some(CRATES_IO),
            Some("https://github.com/org/repo"),
        ));
        root.close().unwrap();
        assert_eq!(
            source.url(Path::new("build.rs")).unwrap(),
            format!("https://github.com/org/repo/blob/{REVISION}/nested/build.rs")
        );
    }

    #[test]
    fn crates_io_fallback_preserves_exact_version_and_encodes_paths() {
        let root = tempfile::tempdir().unwrap();
        for source in [
            CRATES_IO,
            "sparse+https://index.crates.io/",
            "registry+sparse+https://index.crates.io/",
        ] {
            let source = BrowserSource::from_package(&package(root.path(), Some(source), None));
            assert_eq!(
                source.url(Path::new("custom dir/%2F#build.rs")).unwrap(),
                "https://docs.rs/crate/example/1.2.3-beta.4+build.5/source/custom%20dir/%252F%23build.rs"
            );
        }
    }

    #[test]
    fn missing_invalid_and_dirty_provenance_use_only_registry_fallback() {
        let root = tempfile::tempdir().unwrap();
        let package = package(
            root.path(),
            Some(CRATES_IO),
            Some("https://github.com/org/repo"),
        );
        let fallback = "https://docs.rs/crate/example/1.2.3-beta.4+build.5/source/build.rs";
        assert_eq!(
            BrowserSource::from_package(&package)
                .url(Path::new("build.rs"))
                .unwrap(),
            fallback
        );
        for metadata in [
            "not json".to_owned(),
            "{}".to_owned(),
            serde_json::json!({"git": {"sha1": "abcdef"}}).to_string(),
            serde_json::json!({"git": {"sha1": "g".repeat(40)}}).to_string(),
            serde_json::json!({"git": {"sha1": REVISION, "dirty": true}}).to_string(),
            serde_json::json!({"git": {"sha1": REVISION, "dirty": "false"}}).to_string(),
            serde_json::json!({"git": {"sha1": REVISION}, "path_in_vcs": "../other"}).to_string(),
            serde_json::json!({"git": {"sha1": REVISION}, "path_in_vcs": "/other"}).to_string(),
            serde_json::json!({"git": {"sha1": REVISION}, "path_in_vcs": 42}).to_string(),
        ] {
            fs::write(root.path().join(".cargo_vcs_info.json"), metadata).unwrap();
            assert_eq!(
                BrowserSource::from_package(&package)
                    .url(Path::new("build.rs"))
                    .unwrap(),
                fallback
            );
        }
        fs::write(
            root.path().join(".cargo_vcs_info.json"),
            serde_json::json!({"git": {"sha1": REVISION}}).to_string(),
        )
        .unwrap();
        assert!(
            BrowserSource::from_package(&package)
                .url(Path::new("build.rs"))
                .unwrap()
                .starts_with("https://github.com/")
        );
    }

    #[test]
    fn normalizes_supported_github_transports() {
        for repository in [
            "https://github.com/org/repo",
            "http://github.com/org/repo.git/",
            "git://github.com/org/repo.git",
            "ssh://git@github.com/org/repo.git/",
            "ssh://github.com/org/repo",
            "git@github.com:org/repo.git",
            "github.com:org/repo",
            "https://GITHUB.COM/org/repo/",
        ] {
            assert_eq!(
                github_repository(repository).unwrap().as_str(),
                "https://github.com/org/repo",
                "{repository}"
            );
        }
    }

    #[test]
    fn rejects_unsafe_repository_destinations() {
        for repository in [
            "https://github.com.evil.test/org/repo",
            "https://notgithub.com/org/repo",
            "https://github.com@evil.test/org/repo",
            "https://user@github.com/org/repo",
            "https://git:secret@github.com/org/repo",
            "ssh://other@github.com/org/repo",
            "ssh://git:secret@github.com/org/repo",
            "https://github.com:8443/org/repo",
            "file://github.com/org/repo",
            "javascript://github.com/org/repo",
            "https://github.com/org/repo?ref=main",
            "https://github.com/org/repo#main",
            "https://github.com/org/repo/tree/main",
            "https://github.com/org/../repo",
            "https://github.com/org/%2e%2e",
            "https://github.com/org/re%2Fpo",
            "https://github.com/org/repo//",
            "https://github.com//org/repo",
            "https://github.com/org/repo\\other",
            "https://github.com/org/.git",
            "https://github.com/org/..",
        ] {
            assert!(github_repository(repository).is_none(), "{repository}");
        }
    }

    #[test]
    fn never_maps_other_sources_to_docs_rs_by_package_name() {
        let root = tempfile::tempdir().unwrap();
        provenance(root.path(), "");
        for source in [
            None,
            Some("registry+https://private.test/index"),
            Some("sparse+https://private.test/index/"),
            Some("sparse+https://index.crates.io.evil.test/"),
            Some("registry+https://github.com/rust-lang/crates.io-index?other"),
            Some("git+https://github.com/org/repo#main"),
            Some("git+https://github.com/org/repo#0123456789abcdef0123456789abcdef01234567"),
            Some("git+https://private.test/org/repo#0123456789abcdef0123456789abcdef01234567"),
        ] {
            let browser = BrowserSource::from_package(&package(root.path(), source, None));
            assert!(browser.url(Path::new("build.rs")).is_err(), "{source:?}");
        }
        let local = BrowserSource::from_package(&package(
            root.path(),
            None,
            Some("https://github.com/org/repo"),
        ));
        assert!(local.url(Path::new("build.rs")).is_err());
        let private = BrowserSource::from_package(&package(
            root.path(),
            Some("registry+https://private.test/index"),
            Some("https://github.com/org/repo"),
        ));
        assert!(
            private
                .url(Path::new("build.rs"))
                .unwrap()
                .starts_with("https://github.com/")
        );
    }

    #[test]
    fn rejects_paths_that_escape_or_obscure_the_package_root() {
        let root = tempfile::tempdir().unwrap();
        let source = BrowserSource::from_package(&package(root.path(), Some(CRATES_IO), None));
        for path in [
            "",
            "/build.rs",
            "../build.rs",
            "sub/../../build.rs",
            "./build.rs",
            "sub//build.rs",
            "C:/build.rs",
            "sub/./build.rs",
        ] {
            assert!(source.url(Path::new(path)).is_err(), "{path}");
        }
        assert!(
            BrowserSource::default()
                .url(Path::new("build.rs"))
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_script_paths_use_url_separators() {
        let root = tempfile::tempdir().unwrap();
        let source = BrowserSource::from_package(&package(root.path(), Some(CRATES_IO), None));
        assert_eq!(
            source.url(Path::new(r"scripts\custom build.rs")).unwrap(),
            "https://docs.rs/crate/example/1.2.3-beta.4+build.5/source/scripts/custom%20build.rs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_script_paths() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let error = BrowserSource::default()
            .url(Path::new(OsStr::from_bytes(b"build\xff.rs")))
            .unwrap_err();
        assert!(error.to_string().contains("non-UTF-8"));
    }

    #[test]
    fn git_checkout_uses_resolved_source_and_repository_relative_package_path() {
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args([
                    "-c",
                    "user.name=Browser Test",
                    "-c",
                    "user.email=browser@example.test",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap()
        };
        assert!(git(&["init"]).status.success());
        let package_root = root.path().join("crates/space # %");
        fs::create_dir_all(&package_root).unwrap();
        fs::write(
            package_root.join("Cargo.toml"),
            "[package]\nname='example'\nversion='1.2.3'\n",
        )
        .unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(
            git(&["commit", "--no-gpg-sign", "-m", "fixture"])
                .status
                .success()
        );
        let revision = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap();
        let revision = revision.trim();
        for prefix in ["", "?branch=not-a-commit", "?tag=v1.0", "?rev=HEAD"] {
            let source = format!("git+https://github.com/actual/checkout.git{prefix}#{revision}");
            let package = package(
                &package_root,
                Some(&source),
                Some("https://github.com/wrong/manifest"),
            );
            let browser = BrowserSource::from_package(&package);
            assert_eq!(
                browser.url(Path::new("scripts/custom.rs")).unwrap(),
                format!(
                    "https://github.com/actual/checkout/blob/{revision}/crates/space%20%23%20%25/scripts/custom.rs"
                )
            );
        }
        let source = format!("git+https://github.com/actual/checkout#{revision}");
        let browser = BrowserSource::from_package(&package(root.path(), Some(&source), None));
        assert_eq!(
            browser.url(Path::new("build.rs")).unwrap(),
            format!("https://github.com/actual/checkout/blob/{revision}/build.rs")
        );
        for source in [
            format!("git+https://github.com/actual/checkout#{REVISION}"),
            format!("git+https://github.com/actual/checkout?wrong=value#{revision}"),
            format!("git+https://github.com/actual/checkout?branch=a&tag=b#{revision}"),
            "git+https://github.com/actual/checkout#main".to_owned(),
        ] {
            assert!(
                BrowserSource::from_package(&package(&package_root, Some(&source), None))
                    .url(Path::new("build.rs"))
                    .is_err()
            );
        }
    }
}
