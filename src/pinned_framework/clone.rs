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

/// A materialized checkout of the pinned framework revision.
///
/// The guard holds a shared lock on `root/.in-use-<rev>` for as long as the
/// caller works inside the tree: the pruner takes that lock exclusively
/// before removing a superseded revision, so a checkout a stale process is
/// still reading — an orphaned `cargo metadata` from before a pin bump —
/// survives until the process exits instead of being deleted under it.
pub struct PinnedCheckout {
    directory: PathBuf,
    _in_use: std::fs::File,
}

impl std::ops::Deref for PinnedCheckout {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.directory
    }
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
pub fn checkout() -> PinnedCheckout {
    let (git, rev) = source();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/pinned-framework");
    std::fs::create_dir_all(&root).expect("the pinned-framework directory is creatable");
    let lock = std::fs::File::create(root.join(".checkout.lock"))
        .expect("the checkout lock file is creatable");
    // The lock is released by dropping it: a process that dies mid-clone frees
    // it, and the absent `.complete` marker makes the next caller rebuild.
    lock.lock().expect("the checkout lock is taken");
    prune_superseded_checkouts(&root, &rev);
    let directory = root.join(&rev);
    if !directory.join(".complete").is_file() {
        if directory.exists() {
            std::fs::remove_dir_all(&directory)
                .expect("a stale pinned-framework checkout is removable");
        }
        clone_revision(&git, &rev, &directory);
        std::fs::write(directory.join(".complete"), &rev)
            .expect("the completion marker is written");
    }
    // Mark the checkout in use while still under the checkout lock, so no
    // sibling's prune can remove it in between. The marker lives beside the
    // tree, never inside it — deleting a directory that contains a file
    // another process holds open fails outright on Windows.
    let in_use = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.join(format!(".in-use-{rev}")))
        .expect("the in-use marker is creatable");
    fs4::FileExt::lock_shared(&in_use).expect("the in-use marker locks");
    drop(lock);
    PinnedCheckout {
        directory,
        _in_use: in_use,
    }
}

/// Remove checkouts of revisions the pin has moved away from — one stale
/// tree per pin bump otherwise accumulates under the root forever. Runs under
/// the checkout lock, so no other process can be mid-clone on a removed tree,
/// and each candidate's `.in-use-<rev>` marker is try-locked exclusively
/// first: a process still reading that revision holds it shared and the tree
/// stays for the next prune.
fn prune_superseded_checkouts(root: &Path, rev: &str) {
    for entry in std::fs::read_dir(root).expect("the pinned-framework directory is readable") {
        let entry = entry.expect("a pinned-framework entry is readable");
        let path = entry.path();
        if !path.is_dir() || entry.file_name() == std::ffi::OsStr::new(rev) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let marker_path = root.join(format!(".in-use-{name}"));
        let marker = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&marker_path)
            .expect("the in-use marker is creatable");
        match fs4::FileExt::try_lock(&marker) {
            Ok(()) => {
                drop(marker);
                std::fs::remove_dir_all(&path)
                    .expect("a superseded pinned-framework checkout is removable");
                // The marker outlives its tree only until this: nobody holds
                // it (the exclusive lock proved that) and the next prune
                // recreates it on demand.
                let _ = std::fs::remove_file(&marker_path);
            }
            // A stale process still holds the shared lock — the tree stays
            // for the next prune.
            Err(fs4::TryLockError::WouldBlock) => {}
            Err(fs4::TryLockError::Error(error)) => {
                panic!("the in-use marker locks: {error}");
            }
        }
    }
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
