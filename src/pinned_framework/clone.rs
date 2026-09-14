//! The pin itself and a clone of it — the part of the module the `water`
//! binary's tests compile too (`src/terminal/main.rs` includes this file by
//! path), so it must not name the GitHub helpers beside it.

use std::{path::Path, process::Command};

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
