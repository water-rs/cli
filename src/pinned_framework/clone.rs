//! The pin itself and a clone of it — the part of the module the `water`
//! binary's tests compile too (`src/terminal/main.rs` includes this file by
//! path), so it must not name the GitHub helpers beside it.

use std::{path::Path, path::PathBuf, process::Command};

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

/// A shallow clone of the pinned framework revision, submodules included —
/// `cargo metadata` against a member needs the workspace tree complete, so
/// the clone materializes whatever the pin carries.
///
/// The clone is shared: every caller in every test binary gets the same
/// directory under `target/pinned-framework/<rev>`, materialized once behind
/// a file lock and kept for the next run's cache. The previous per-caller
/// blob-less clone made every ignored test fetch and expand the same tree at
/// once, and a blob-less checkout pays a lazy fetch per file — five of them
/// racing did not finish inside the nightly job's per-test timeout on
/// Windows.
pub fn checkout() -> PathBuf {
    let (git, rev) = source();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/pinned-framework");
    std::fs::create_dir_all(&root).expect("the pinned-framework directory is creatable");
    let lock = std::fs::File::create(root.join(".checkout.lock"))
        .expect("the checkout lock file is creatable");
    // The lock is released by dropping it: a process that dies mid-clone frees
    // it, and the absent `.complete` marker makes the next caller rebuild.
    lock.lock().expect("the checkout lock is taken");
    let directory = root.join(&rev);
    if !directory.join(".complete").is_file() {
        let _ = std::fs::remove_dir_all(&directory);
        clone_revision(&git, &rev, &directory);
        std::fs::write(directory.join(".complete"), &rev)
            .expect("the completion marker is written");
    }
    drop(lock);
    directory
}

/// Fetch exactly `rev` and expand it, submodules included. A depth-1 fetch of
/// the pinned commit downloads the complete tree in one pack — no lazy blob
/// requests during checkout, no history nobody reads.
fn clone_revision(git: &str, rev: &str, directory: &Path) {
    std::fs::create_dir_all(directory).expect("the clone directory is creatable");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(["-c", "advice.detachedHead=false"])
            .args(args)
            .current_dir(directory)
            .status()
            .expect("git must spawn");
        assert!(
            status.success(),
            "git {} failed for {git}@{rev}",
            args.join(" ")
        );
    };
    run(&["init", "--quiet"]);
    run(&["remote", "add", "origin", git]);
    run(&["fetch", "--depth", "1", "origin", rev]);
    run(&["checkout", "--detach", "FETCH_HEAD"]);
    run(&[
        "submodule",
        "update",
        "--init",
        "--recursive",
        "--depth",
        "1",
    ]);
}
