//! Symbols embedded in compiled Rust artifacts.
//!
//! Procedural macros report metadata to the CLI through `#[used] static` items
//! whose mangled names carry a `waterui_meta_*` leaf, and through plain exports
//! such as `waterui_preview_*`. Both are recovered here by enumerating the
//! artifact's symbol table, which is ground truth: macros, cfgs, and generics
//! are already resolved in it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cargo_metadata::TargetKind;
use color_eyre::eyre::{Context as _, Result, bail};
use object::read::archive::ArchiveFile;
use object::{File, FileKind, Object, ObjectSection, ObjectSymbol};
use waterui_assets_planner::BundleMountMeta;

use crate::build::{BuildProgress, command_output_with_progress};

/// Symbols of one compiled Rust artifact: an rlib/staticlib archive (every
/// member parsed) or a single object/dylib.
pub struct ArtifactSymbols {
    data: Vec<u8>,
    names: Vec<String>,
}

impl ArtifactSymbols {
    /// Read every symbol of the artifact at `path`.
    ///
    /// Archive members that do not parse as object files (for example
    /// `lib.rmeta`) are skipped silently.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read or no part of it parses
    /// as a recognized object or archive.
    pub fn read(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .wrap_err_with(|| format!("failed to read artifact {}", path.display()))?;
        let mut names = Vec::new();
        let mut objects = 0usize;
        for_each_object(&data, |file| {
            objects += 1;
            names.extend(
                file.symbols()
                    .filter_map(|symbol| symbol.name().ok())
                    .map(demangled_name),
            );
        });
        if objects == 0 {
            bail!("{} is not a recognized object or archive", path.display());
        }
        Ok(Self { data, names })
    }

    /// Demangled symbol names.
    ///
    /// Names are demangled with the `{:#}` alternate form so trailing hashes
    /// are dropped; un-mangled names pass through unchanged.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// Leaf path segments (text after the last `::`, the whole name when it
    /// has none) that start with `prefix`. Deduplicated and sorted.
    pub fn leaves_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.names()
            .filter_map(leaf_of)
            .filter(|leaf| leaf.starts_with(prefix))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The bundle-mount metadata a `waterui_meta_bundle_*` static carries.
    ///
    /// The macro records the mount and project roots with
    /// `std::fs::canonicalize`, which on Windows spells them as verbatim
    /// `\\?\C:\...` paths. Those are valid for the standard library but not
    /// for what the CLI hands them to — a frontend package manager's working
    /// directory, Xcode and Gradle inputs, equality against paths the CLI
    /// resolved itself — so both roots are simplified to their ordinary
    /// spelling here, at the one place the payload enters the CLI.
    ///
    /// # Errors
    /// Returns an error when the static is missing or its payload does not
    /// decode.
    pub fn bundle_mount_meta(&self, leaf: &str) -> Result<BundleMountMeta> {
        let mut meta = BundleMountMeta::from_payload(&self.static_bytes(leaf)?)?;
        meta.path = dunce::simplified(&meta.path).to_path_buf();
        meta.project = meta
            .project
            .map(|project| dunce::simplified(&project).to_path_buf());
        Ok(meta)
    }

    /// Bytes of a `#[used] static`.
    ///
    /// Locates the symbol whose demangled leaf equals `leaf`, reads its
    /// section data from the symbol address, and cuts at the first NUL byte:
    /// payloads are NUL-terminated by contract because Mach-O symbols carry
    /// no size.
    ///
    /// # Errors
    /// Returns an error when no symbol has that leaf or when two different
    /// definitions exist.
    pub fn static_bytes(&self, leaf: &str) -> Result<Vec<u8>> {
        let mut payloads = BTreeSet::new();
        for_each_object(&self.data, |file| {
            for symbol in file.symbols() {
                let Ok(raw) = symbol.name() else { continue };
                if leaf_of(&demangled_name(raw)) != Some(leaf) {
                    continue;
                }
                let Some(index) = symbol.section_index() else {
                    continue;
                };
                let Ok(section) = file.section_by_index(index) else {
                    continue;
                };
                let Ok(data) = section.data() else { continue };
                let Ok(offset) = usize::try_from(symbol.address().wrapping_sub(section.address()))
                else {
                    continue;
                };
                if let Some(bytes) = data.get(offset..) {
                    payloads.insert(
                        bytes
                            .split(|byte| *byte == 0)
                            .next()
                            .unwrap_or_default()
                            .to_vec(),
                    );
                }
            }
        });
        let mut payloads = payloads.into_iter();
        let Some(payload) = payloads.next() else {
            bail!("no symbol with leaf `{leaf}` carries section data in the artifact");
        };
        if payloads.next().is_some() {
            bail!("artifact defines `{leaf}` more than once with different payloads");
        }
        Ok(payload)
    }
}

impl std::fmt::Debug for ArtifactSymbols {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactSymbols")
            .field("symbols", &self.names.len())
            .finish_non_exhaustive()
    }
}

/// Invoke `f` on every object file contained in `data`: each member of an
/// archive, or `data` itself when it is a single object/dylib.
fn for_each_object<'a>(data: &'a [u8], mut f: impl FnMut(File<'a>)) {
    if matches!(FileKind::parse(data), Ok(FileKind::Archive))
        && let Ok(archive) = ArchiveFile::parse(data)
    {
        for member in archive.members().flatten() {
            if member.name() == b"lib.rmeta" {
                continue;
            }
            if let Ok(member_data) = member.data(data)
                && let Ok(file) = File::parse(member_data)
            {
                f(file);
            }
        }
        return;
    }
    if let Ok(file) = File::parse(data) {
        f(file);
    }
}

/// Demangle a raw symbol name, dropping the trailing disambiguation hash.
///
/// Mach-O prepends `_` to every external symbol; the legacy/v0 manglings
/// absorb it during demangling, but `#[no_mangle]` names keep it, so it is
/// stripped afterwards.
fn demangled_name(raw: &str) -> String {
    let demangled = format!("{:#}", rustc_demangle::demangle(raw));
    demangled
        .strip_prefix('_')
        .map_or_else(|| demangled.clone(), str::to_owned)
}

/// The leaf segment of a possibly-qualified demangled name.
fn leaf_of(name: &str) -> Option<&str> {
    name.rsplit("::").next().filter(|leaf| !leaf.is_empty())
}

/// Build the project's library crate for the host (debug) and return the path
/// of the produced `.rlib`.
///
/// Runs `cargo build --lib --message-format=json-render-diagnostics` with
/// `project_path` as the working directory and `target_dir` as the explicit
/// Cargo target directory — callers pass the CLI's shared per-user target so
/// the dependency graph compiles once per machine rather than once per
/// project. `sccache_path`, when given, is installed as `RUSTC_WRAPPER`
/// through the same helper every other CLI build uses. `progress`, when
/// given, receives cargo's compile events — the same streaming a
/// [`crate::build::RustBuild`] reports — because this compile is often the
/// first thing `water run` does and a cold one takes minutes.
///
/// # Errors
/// Returns an error when cargo fails or the project produces no rlib.
pub async fn build_host_rlib(
    project_path: &Path,
    target_dir: &Path,
    sccache_path: Option<&Path>,
    progress: Option<&BuildProgress>,
) -> Result<PathBuf> {
    let manifest_path = dunce::canonicalize(project_path.join("Cargo.toml"))
        .wrap_err_with(|| format!("no Cargo.toml under {}", project_path.display()))?;

    let mut cargo = smol::process::Command::new("cargo");
    cargo
        .args(["build", "--lib", "--message-format=json-render-diagnostics"])
        .arg("--target-dir")
        .arg(target_dir)
        .current_dir(project_path);
    if let Some(sccache_path) = sccache_path {
        crate::toolchain::sccache::configure_compilation_cache(&mut cargo, sccache_path)?;
    }
    // Stdout stays collected-only: it carries the JSON message stream parsed
    // below, so a progress sink must never mirror it to the terminal.
    let output = command_output_with_progress(&mut cargo, progress.cloned())
        .await
        .wrap_err("failed to execute `cargo build --lib`")?;
    if !output.status.success() {
        bail!(
            "`cargo build --lib` failed in {}:\n{}",
            project_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    for artifact in crate::build::compiler_artifacts(&output.stdout)
        .wrap_err("failed to parse cargo build messages")?
    {
        if !crate::build::same_manifest_path(artifact.manifest_path.as_std_path(), &manifest_path)
            || !artifact
                .target
                .kind
                .iter()
                .any(|kind| matches!(kind, TargetKind::Lib | TargetKind::RLib))
        {
            continue;
        }
        if let Some(rlib) = artifact.filenames.iter().find(|filename| {
            filename
                .as_std_path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rlib"))
        }) {
            return Ok(rlib.clone().into_std_path_buf());
        }
    }
    bail!(
        "`cargo build --lib` produced no rlib for {}",
        manifest_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_meta_statics_and_exports_from_built_rlib() {
        futures_lite::future::block_on(async {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/meta_static");
            let rlib = build_host_rlib(&fixture, &fixture.join("target"), None, None)
                .await
                .expect("fixture crate should build");
            let symbols = ArtifactSymbols::read(&rlib).expect("rlib should parse");
            let previews = symbols.leaves_with_prefix("waterui_preview_");
            assert_eq!(previews, ["waterui_preview_meta_static_probe"]);
            assert_eq!(
                symbols
                    .static_bytes("waterui_meta_test_probe")
                    .expect("static should be present"),
                b"hello"
            );
        });
    }

    /// `#[used]` is linker-retained (`no_dead_strip` on Mach-O), so every
    /// `waterui_meta_*` static is `#[cfg(debug_assertions)]`: a release rlib
    /// must carry none. Discovery never reads the target build anyway — the
    /// CLI builds a dev-profile host rlib.
    #[test]
    fn release_rlib_carries_no_meta_statics() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/meta_static");
        let status = std::process::Command::new("cargo")
            .args(["build", "--lib", "--release"])
            .current_dir(&fixture)
            .status()
            .expect("cargo build --release runs");
        assert!(status.success(), "the release fixture build must succeed");
        let symbols = ArtifactSymbols::read(&fixture.join("target/release/libmeta_static.rlib"))
            .expect("release rlib should parse");
        assert!(
            symbols.leaves_with_prefix("waterui_meta_").is_empty(),
            "a release rlib must not carry waterui_meta_* statics"
        );
    }

    /// The `web_meta` fixture staged against the pinned framework: its sources
    /// copied beside a manifest whose `waterui` dependency is the git pin this
    /// crate's own manifest carries, so the revision lives in one place.
    ///
    /// The fixture lives under `target/test-fixtures/` rather than a tempdir:
    /// its `cargo build --lib` compiles the pinned framework's graph, which is
    /// the expensive part, and a persistent target dir lets a nextest retry —
    /// and the next cached CI run — resume that compile instead of restarting
    /// it cold every attempt. Sources stage into a wiped `crate/` beside the
    /// persistent `target/` so a file deleted from `tests/fixtures/web_meta`
    /// cannot survive in the staged tree.
    ///
    /// The returned file handle is a held lock covering the fixture for the
    /// whole test: a second caller's restage waits rather than wiping `crate/`
    /// under this one's build. `crate/Cargo.lock` survives the wipe on
    /// purpose — it is a build artifact, not fixture content, and regenerating
    /// it re-resolves the pinned git dependency on every retry.
    fn web_meta_fixture() -> (PathBuf, std::fs::File) {
        let sources = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/web_meta");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures/web-meta");
        std::fs::create_dir_all(&fixture).expect("the fixture directory is creatable");
        let lock_path = fixture.join(".restage.lock");
        let restage_lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .expect("the restage lock opens");
        fs4::FileExt::lock(&restage_lock).expect("the restage lock acquires");

        let staged = fixture.join("crate");
        for entry in std::fs::read_dir(&fixture).expect("the fixture directory is readable") {
            let path = entry.expect("a fixture entry is readable").path();
            if path == fixture.join("target") || path == staged || path == lock_path {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            }
            .expect("a stale fixture entry is removable");
        }
        // `crate/` itself is wiped too, except `Cargo.lock` — the one build
        // artifact worth keeping across a restage.
        if staged.is_dir() {
            for entry in std::fs::read_dir(&staged).expect("the staged crate is readable") {
                let path = entry.expect("a staged entry is readable").path();
                if path == staged.join("Cargo.lock") {
                    continue;
                }
                if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                }
                .expect("a stale staged entry is removable");
            }
        }
        fs_extra::dir::copy(
            &sources,
            &staged,
            &fs_extra::dir::CopyOptions::new()
                .content_only(true)
                .overwrite(true),
        )
        .expect("the fixture sources copy");
        std::fs::write(staged.join("Cargo.toml"), web_meta_manifest())
            .expect("the manifest is written");
        (fixture, restage_lock)
    }

    /// The staged fixture's `Cargo.toml`, with the `waterui` dependency pinned
    /// to the revision this crate's own manifest carries — the revision lives
    /// in one place.
    fn web_meta_manifest() -> String {
        #[derive(serde::Serialize)]
        struct Manifest {
            package: Package,
            workspace: toml::Table,
            dependencies: std::collections::BTreeMap<&'static str, Dependency>,
            patch: Patch,
            profile: Profile,
        }
        #[derive(serde::Serialize)]
        struct Patch {
            #[serde(rename = "crates-io")]
            crates_io: std::collections::BTreeMap<&'static str, GitSource>,
        }
        #[derive(serde::Serialize)]
        struct GitSource {
            git: String,
            rev: String,
        }
        #[derive(serde::Serialize)]
        struct Package {
            name: &'static str,
            version: &'static str,
            edition: &'static str,
        }
        #[derive(serde::Serialize)]
        struct Dependency {
            git: String,
            rev: String,
            #[serde(rename = "default-features")]
            default_features: bool,
            features: Vec<&'static str>,
        }
        #[derive(serde::Serialize)]
        struct Profile {
            dev: DevProfile,
        }
        /// The rlib is read for `waterui_meta_*` statics, which live behind
        /// `debug_assertions` — debug info itself buys the test nothing, and
        /// emitting it for the whole framework graph is a real slice of a cold
        /// build's time.
        #[derive(serde::Serialize)]
        struct DevProfile {
            debug: u8,
        }

        let (git, rev) = crate::pinned_framework::source();
        // The lean facade: `include_web!` expands against `waterui::webview`
        // and `waterui::Bundle`, nothing else of the framework is needed.
        let manifest = Manifest {
            package: Package {
                name: "web-meta",
                version: "0.0.0",
                edition: "2024",
            },
            workspace: toml::Table::new(),
            dependencies: std::iter::once((
                "waterui",
                Dependency {
                    git: git.clone(),
                    rev: rev.clone(),
                    default_features: false,
                    features: vec!["webview", "assets"],
                },
            ))
            .collect(),
            // The extracted `waterui-image` the facade links from crates.io
            // depends on the registry copies of these crates; without the
            // redirect the graph carries two of each and every `View` is a
            // different type on either side.
            patch: Patch {
                crates_io: [
                    "waterui-core",
                    "waterui-graphics",
                    "waterui-layout",
                    "waterui-macros",
                ]
                .into_iter()
                .map(|name| {
                    (
                        name,
                        GitSource {
                            git: git.clone(),
                            rev: rev.clone(),
                        },
                    )
                })
                .collect(),
            },
            profile: Profile {
                dev: DevProfile { debug: 0 },
            },
        };
        toml::to_string(&manifest).expect("the manifest serializes")
    }

    /// `include_web!` is the one web mount an application declares; its
    /// metadata must reach the CLI through the same `waterui_meta_bundle_*`
    /// channel a plain `include_bundle!` uses, carrying the frontend project
    /// root so `water run` knows what to build (#587). The macro expands
    /// against the `waterui` facade, which this crate does not link, so cargo
    /// fetches the pinned framework revision to build the fixture — network
    /// work that belongs to the nightly job.
    #[test]
    #[ignore = "fetches the pinned framework revision"]
    fn reads_include_web_mount_meta_from_built_rlib() {
        futures_lite::future::block_on(async {
            let (fixture, _restage_guard) = web_meta_fixture();
            let project = fixture.join("crate");
            let rlib = build_host_rlib(&project, &fixture.join("target"), None, None)
                .await
                .expect("fixture crate should build");
            let symbols = ArtifactSymbols::read(&rlib).expect("rlib should parse");
            let meta = symbols
                .bundle_mount_meta("waterui_meta_bundle_web")
                .expect("payload should decode as BundleMountMeta");
            assert_eq!(meta.mount, "web");
            assert!(
                meta.path.ends_with("dist"),
                "the default out dir is dist: {}",
                meta.path.display()
            );
            assert_eq!(
                meta.project.as_deref(),
                Some(
                    dunce::canonicalize(project.join("web"))
                        .as_deref()
                        .expect("web root")
                )
            );
        });
    }
}
