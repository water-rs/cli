//! `water package` must observe the backend it compiles.
//!
//! The managed `DerivedData` persists between packages, while the generated
//! Xcode project records the local backend's identity as its path alone.
//! This exercises the defect end to end: change the backend's sources at
//! the same path between two packages — the second must rebuild it rather
//! than reuse the previous build's intermediates.
//!
//! Clones the framework and the backend checkout and drives `xcodebuild`;
//! marked `#[ignore]` like every test needing a framework checkout or the
//! network (nightly only).

#![cfg(target_os = "macos")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const MARKER_BEFORE: &str = "water_package_cache_probe_marker_before";
const MARKER_AFTER: &str = "water_package_cache_probe_marker_after";

/// The water-rs/waterui revision this crate's manifest pins, read back from
/// `Cargo.lock` — the same commit the framework's git dependencies resolve
/// to, so the test exercises the pairing `water` ships.
fn pinned_framework_revision() -> String {
    let lock = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"))
        .expect("read Cargo.lock");
    lock.lines()
        .filter_map(|line| line.strip_prefix("source = \"git+https://github.com/water-rs/waterui"))
        .find_map(|source| {
            source
                .rsplit('#')
                .next()
                .map(|sha| sha.trim_end_matches('"').to_owned())
        })
        .expect("Cargo.lock records the pinned framework revision")
}

/// The backend ref the cloned framework manifest pins —
/// `apple-backend-revision` on a dev channel, else the release's
/// `apple-backend-version` tag.
fn pinned_backend_ref(waterui: &Path) -> String {
    let manifest =
        fs::read_to_string(waterui.join("Cargo.toml")).expect("read the framework manifest");
    for key in ["apple-backend-revision", "apple-backend-version"] {
        if let Some(value) = manifest.lines().find_map(|line| {
            let (field, value) = line.split_once('=')?;
            (field.trim() == key).then(|| value.trim().trim_matches('"').to_owned())
        }) {
            return value;
        }
    }
    panic!("the framework manifest pins no apple backend ref")
}

/// A shallow clone of `url` at `git_ref` — a commit sha or a version tag —
/// inside `dir`.
fn clone_at(dir: &Path, url: &str, git_ref: &str) {
    fs::create_dir_all(dir).expect("create the clone dir");
    git(dir, &["init"]);
    git(dir, &["remote", "add", "origin", url]);
    for candidate in [git_ref.to_owned(), format!("refs/tags/{git_ref}")] {
        let status = Command::new("git")
            .args(["fetch", "--depth", "1", "origin", &candidate])
            .current_dir(dir)
            .output()
            .expect("git fetch");
        if status.status.success() {
            git(dir, &["checkout", "FETCH_HEAD"]);
            return;
        }
    }
    panic!("no ref `{git_ref}` on {url}");
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to launch {program}: {e}"));
    assert!(
        output.status.success(),
        "`{program} {}` failed:\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn git(dir: &Path, args: &[&str]) {
    run(dir, "git", args);
}

fn water(dir: &Path, args: &[&str]) {
    run(dir, env!("CARGO_BIN_EXE_water"), args);
}

/// The packaged executable inside `project/target/package`.
fn packaged_binary(project: &Path) -> PathBuf {
    let mut apps = fs::read_dir(project.join("target/package"))
        .expect("the packaged output dir")
        .filter_map(|entry| {
            let path = entry.expect("dir entry").path();
            (path.extension().is_some_and(|ext| ext == "app")).then_some(path)
        })
        .collect::<Vec<_>>();
    apps.sort();
    assert_eq!(apps.len(), 1, "expected exactly one packaged app");
    let binaries = apps[0].join("Contents/MacOS");
    let binaries = fs::read_dir(&binaries)
        .expect("the app bundle's MacOS dir")
        .map(|entry| entry.expect("dir entry").path())
        .collect::<Vec<_>>();
    assert_eq!(binaries.len(), 1, "expected exactly one app executable");
    binaries[0].clone()
}

fn backend_checkout(root: &Path) -> PathBuf {
    root.join("waterui/backends/apple")
}

fn write_probe(backend: &Path, marker: &str) {
    fs::write(
        backend.join("Sources/WaterUI/PackageCacheProbe.swift"),
        format!("enum WaterPackageCacheProbe {{ static let marker = \"{marker}\" }}\n"),
    )
    .expect("write the backend probe source");
}

fn binary_contains(binary: &Path, marker: &str) -> bool {
    let bytes = fs::read(binary).expect("read the packaged binary");
    bytes
        .windows(marker.len())
        .any(|window| window == marker.as_bytes())
}

/// Changing the backend at the same path between two `water package` runs
/// must invalidate the cache the first run left: the second package
/// rebuilds and ships the new backend, never the stale artifact.
#[test]
#[ignore = "clones the framework and backend checkouts and runs Xcode"]
fn repackaging_after_a_backend_change_rebuilds_the_backend() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();

    let waterui = root.join("waterui");
    clone_at(
        &waterui,
        "https://github.com/water-rs/waterui",
        &pinned_framework_revision(),
    );

    // The pinned tree carries `backends/apple` as an unmaterialized
    // gitlink; place a real checkout there at the revision the framework
    // manifest pins, the way a developer's local checkout lands there.
    let backend = backend_checkout(root);
    fs::create_dir_all(backend.parent().expect("backends dir")).expect("create backends dir");
    clone_at(
        &backend,
        "https://github.com/water-rs/apple-backend",
        &pinned_backend_ref(&waterui),
    );
    write_probe(&backend, MARKER_BEFORE);

    water(
        root,
        &[
            "create",
            "probe",
            "--mode",
            "playground",
            "--waterui-path",
            root.join("waterui").to_str().expect("utf8 path"),
        ],
    );
    let project = root.join("probe");
    let package_args = ["package", "--platform", "macos", "--backend", "apple"];

    water(&project, &package_args);
    let binary = packaged_binary(&project);
    assert!(
        binary_contains(&binary, MARKER_BEFORE),
        "the first package must ship the checkout's sources"
    );

    // Change the backend between the two packages at the same path.
    write_probe(&backend, MARKER_AFTER);
    water(&project, &package_args);
    let binary = packaged_binary(&project);
    assert!(
        binary_contains(&binary, MARKER_AFTER),
        "the second package must ship the edited backend, not the stale artifact"
    );
    assert!(
        !binary_contains(&binary, MARKER_BEFORE),
        "the second package must not keep objects from the previous backend"
    );
}
