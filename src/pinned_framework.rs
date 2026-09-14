//! Access to the `water-rs/waterui` revision this crate is pinned against.
//!
//! The manifest's git dependencies are the single source of truth for the
//! pin; the tests that need the real framework source — a `cargo metadata`
//! graph over real manifests, a checkout layout, a file at the pinned commit —
//! read it back from `Cargo.toml` rather than carrying a second copy. Every
//! caller is `#[ignore]`d and runs in the nightly job: cloning or fetching is
//! network-bound and far too slow for a normal `cargo test`.

use std::{path::Path, process::Command};

use zenwave::Client as _;

/// The framework repository URL and the revision the `waterui-*` git
/// dependencies in this crate's manifest pin. Every `waterui-*` dependency
/// must share one `git` and one `rev` — `build.rs` enforces the same rule —
/// so the pin is read once and answers for all of them.
pub fn source() -> (String, String) {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let contents = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest_path.display()));
    let manifest: toml::Value = toml::from_str(&contents)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", manifest_path.display()));
    let mut sources = std::collections::BTreeSet::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(table) = manifest.get(section).and_then(toml::Value::as_table) else {
            continue;
        };
        for (name, dependency) in table {
            if !name.starts_with("waterui-") {
                continue;
            }
            let git = dependency
                .get("git")
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("[{section}] {name} must be a git dependency"));
            let rev = dependency
                .get("rev")
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("[{section}] {name} must pin a rev"));
            sources.insert((git.to_owned(), rev.to_owned()));
        }
    }
    assert!(
        sources.len() == 1,
        "Cargo.toml must pin every waterui-* dependency on one git source at one rev, found {sources:?}"
    );
    sources
        .pop_first()
        .expect("the length check leaves one source")
}

/// The `<owner>/<repo>` slug of a `https://github.com/…` remote.
fn github_slug(git: &str) -> &str {
    git.trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")
        .unwrap_or_else(|| panic!("{git} is not a github.com remote"))
}

/// A blob-less clone of the pinned framework revision, submodules included —
/// the workspace `[patch]` table names `kit/` paths, so `cargo metadata`
/// against any member needs them checked out.
pub fn checkout() -> tempfile::TempDir {
    let (git, rev) = source();
    let directory = tempfile::tempdir().expect("framework clone directory");
    let status = Command::new("git")
        .args([
            "-c",
            "advice.detachedHead=false",
            "clone",
            "--filter=blob:none",
            "--recurse-submodules",
            "--revision",
            &rev,
            &git,
        ])
        .arg(directory.path())
        .status()
        .expect("git must spawn");
    assert!(status.success(), "cloning water-rs/waterui@{rev} failed");
    directory
}

/// `GET`s `url` and returns the body, asserting a success status.
pub fn fetch(url: &str) -> Vec<u8> {
    smol::block_on(async {
        let mut client = zenwave::client();
        let response = client
            .method(zenwave::Method::GET, url)
            .and_then(|request| request.header("User-Agent", env!("CARGO_PKG_NAME")))
            .unwrap_or_else(|error| panic!("invalid request to {url}: {error}"))
            .await
            .unwrap_or_else(|error| panic!("GET {url} failed: {error}"));
        assert!(
            response.status().is_success(),
            "GET {url} returned HTTP {}",
            response.status()
        );
        response
            .into_body()
            .into_bytes()
            .await
            .unwrap_or_else(|error| panic!("reading {url} failed: {error}"))
            .to_vec()
    })
}

/// The `sha` and `submodule_git_url` of the gitlink at `path` in the
/// repository `git` names, at `rev`, resolved through the GitHub contents API.
pub fn gitlink(git: &str, path: &str, rev: &str) -> (String, String) {
    let url = format!(
        "https://api.github.com/repos/{}/contents/{path}?ref={rev}",
        github_slug(git)
    );
    let entry: serde_json::Value = serde_json::from_slice(&fetch(&url))
        .unwrap_or_else(|error| panic!("contents API for {path}@{rev} returned no JSON: {error}"));
    let sha = entry["sha"]
        .as_str()
        .unwrap_or_else(|| panic!("contents API entry for {path}@{rev} carries no sha"));
    let submodule = entry["submodule_git_url"]
        .as_str()
        .unwrap_or_else(|| panic!("{path}@{rev} is not a submodule gitlink"));
    (sha.to_owned(), submodule.to_owned())
}

/// The `raw.githubusercontent.com` URL of `path` at `rev` in the repository
/// `git` names.
pub fn raw_url(git: &str, rev: &str, path: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{rev}/{path}",
        github_slug(git)
    )
}
