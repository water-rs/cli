//! Access to the `water-rs/waterui` revision this crate is pinned against.
//!
//! The manifest's git dependencies are the single source of truth for the
//! pin; the tests that need the real framework source — a `cargo metadata`
//! graph over real manifests, a checkout layout, a file at the pinned commit —
//! read it back from `Cargo.toml` rather than carrying a second copy. Every
//! caller is `#[ignore]`d and runs in the nightly job: cloning or fetching is
//! network-bound and far too slow for a normal `cargo test`.

use zenwave::Client as _;

mod clone;
pub use clone::{checkout, source};

/// The `<owner>/<repo>` slug of a `https://github.com/…` remote.
fn github_slug(git: &str) -> &str {
    git.trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")
        .unwrap_or_else(|| panic!("{git} is not a github.com remote"))
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

/// The `(commit, repository)` of the Android runtime the framework at `rev`
/// builds against: the `android-backend-revision` / `android-backend-url`
/// literals its root manifest declares, or — for a revision from before
/// water-rs/waterui#940 declared them — the `backends/android` gitlink.
pub fn android_backend(git: &str, rev: &str) -> (String, String) {
    let manifest = fetch(&raw_url(git, rev, "Cargo.toml"));
    let manifest: toml::Value =
        toml::from_str(std::str::from_utf8(&manifest).unwrap_or_else(|error| {
            panic!("the framework manifest at {rev} is not UTF-8: {error}")
        }))
        .unwrap_or_else(|error| panic!("the framework manifest at {rev} is not TOML: {error}"));
    let metadata = &manifest["package"]["metadata"]["waterui"];
    match (
        metadata
            .get("android-backend-revision")
            .and_then(toml::Value::as_str),
        metadata
            .get("android-backend-url")
            .and_then(toml::Value::as_str),
    ) {
        (Some(revision), Some(url)) => (revision.to_owned(), url.to_owned()),
        (None, _) => gitlink(git, "backends/android", rev),
        (Some(_), None) => panic!(
            "the framework manifest at {rev} declares android-backend-revision without android-backend-url"
        ),
    }
}
