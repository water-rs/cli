//! Build system

use std::{
    ffi::OsString,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
};

use eyre::{Context as _, bail};
use futures_util::StreamExt as _;
use smol::{io::AsyncReadExt as _, process::Command, unblock};
use target_lexicon::{Environment, OperatingSystem, Triple};

use crate::project::Project;
use crate::utils::{run_command, std_output_enabled};

/// Get the dynamic library extension for a target triple.
#[must_use]
pub const fn lib_extension_for_triple(triple: &Triple) -> &'static str {
    match triple.operating_system {
        OperatingSystem::Darwin(_)
        | OperatingSystem::MacOSX { .. }
        | OperatingSystem::IOS(_)
        | OperatingSystem::TvOS(_)
        | OperatingSystem::WatchOS(_)
        | OperatingSystem::VisionOS(_) => "dylib",
        OperatingSystem::Windows => "dll",
        // Linux, Android, and most other Unix-like targets use .so.
        _ => "so",
    }
}

/// Resolve the Rust standard-library directory for a target triple.
///
/// # Errors
/// Returns an error if rustc cannot resolve an existing target library directory.
pub async fn rust_target_libdir(triple: &Triple) -> eyre::Result<PathBuf> {
    let target = triple.to_string();
    let output = run_command(
        "rustc",
        ["--print", "target-libdir", "--target", target.as_str()],
    )
    .await?;
    let libdir = output.trim();
    if libdir.is_empty() {
        bail!("`rustc --print target-libdir --target {target}` returned an empty path");
    }
    let path = PathBuf::from(libdir);
    if !path.is_dir() {
        bail!(
            "Rust target libdir does not exist for dynamic linking: {}",
            path.display()
        );
    }
    Ok(path)
}

/// The Cargo target a build selects.
///
/// A crate-type override only has meaning for the library target, so carrying the
/// target kind in the type keeps `cargo rustc -- --crate-type` from ever reaching a
/// binary build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CargoTarget<'a> {
    /// The crate's library target.
    Lib,
    /// One named binary target.
    Binary(&'a str),
}

impl<'a> CargoTarget<'a> {
    fn cargo_args(self) -> Vec<&'a str> {
        match self {
            Self::Lib => vec!["--lib"],
            Self::Binary(name) => vec!["--bin", name],
        }
    }

    const fn accepts_crate_type_override(self) -> bool {
        matches!(self, Self::Lib)
    }
}

/// Selects how Rust dependencies are linked into a native application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustLinkage {
    /// Link the `WaterUI` runtime into the application archive.
    Static,
    /// Link the application and loadable modules against one shared `WaterUI` runtime.
    SharedRuntime,
}

/// Configure a Cargo invocation that compiles one of `WaterUI`'s generated crates.
///
/// Incremental compilation is off for every one of these builds, unconditionally.
/// `-C incremental` is part of a unit's profile, the profile feeds Cargo's `-C metadata`,
/// and `-C metadata` is mangled into every symbol name. Two builds in the same flow that
/// disagree about incremental therefore produce runtimes whose symbols cannot resolve
/// against each other: a preview support app built one way and a preview module built the
/// other share a `libwaterui_dylib.dylib` filename and roughly 33,000 mismatched symbols,
/// and the module fails to `dlopen` on a missing generic instantiation.
///
/// The choice is unconditional precisely so it cannot depend on an environmental accident
/// such as whether a machine has `sccache` installed. Little is given up: every generated
/// backend builds into one shared target directory where Cargo already reuses each unit's
/// compiled artifact across backends and feature variants — while an `sccache` entry,
/// which requires incremental to be off, covers what that sharing cannot.
pub fn configure_generated_crate_compilation(command: &mut Command) {
    command.env("CARGO_INCREMENTAL", "0");
}

/// Dynamic Rust libraries required by a shared-runtime development build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustDynamicLibraries {
    waterui: PathBuf,
    standard_library: PathBuf,
    triple: Triple,
}

impl RustDynamicLibraries {
    /// Resolve the shared `WaterUI` runtime and target Rust standard library.
    ///
    /// # Errors
    /// Returns an error when either required dynamic library is absent or ambiguous.
    pub async fn resolve(lib_dir: &Path, triple: &Triple) -> eyre::Result<Self> {
        let file_name = dynamic_library_file_name("waterui_dylib", triple);
        // Cargo emits a dependency's final dylib artifact in `deps/` on stable
        // and at the profile directory root on current nightlies; accept both.
        // `deps/` wins: a copy an earlier `stage` left at the profile root must
        // never mask the artifact the current build produced.
        let waterui = [
            lib_dir.join("deps").join(&file_name),
            lib_dir.join(&file_name),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            eyre::eyre!(
                "Shared WaterUI runtime was not built at {}",
                lib_dir.join("deps").join(&file_name).display()
            )
        })?;

        // A `-Zbuild-std` build publishes its freshly compiled `libstd` into
        // the profile's `deps/` directory via the rustc wrapper; that copy —
        // not the toolchain's prebuilt one — is what the build linked against,
        // so it is the one that has to ship. The prebuilt lookup below is the
        // fallback for builds that never built `std` from source.
        let resolution_triple = triple.clone();
        let deps_dir = lib_dir.join("deps");
        let staged =
            unblock(move || resolve_rust_standard_library_in(&deps_dir, &resolution_triple)).await;
        let standard_library = match staged {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let target_libdir = rust_target_libdir(triple).await?;
                let resolution_triple = triple.clone();
                unblock(move || {
                    resolve_rust_standard_library_in(&target_libdir, &resolution_triple)
                })
                .await?
            }
            Err(error) => return Err(error.into()),
        };

        Ok(Self {
            waterui,
            standard_library,
            triple: triple.clone(),
        })
    }

    /// Shared `WaterUI` runtime path.
    #[must_use]
    pub fn waterui(&self) -> &Path {
        &self.waterui
    }

    /// Target Rust standard-library dynamic library path.
    #[must_use]
    pub fn standard_library(&self) -> &Path {
        &self.standard_library
    }

    /// Iterate over every library that must be staged with the application.
    pub fn iter(&self) -> impl Iterator<Item = &Path> {
        [self.waterui(), self.standard_library()].into_iter()
    }

    /// Copy all required dynamic libraries into a runtime search directory.
    ///
    /// Staging goes through the reflinking copy so a shared runtime that every build
    /// output needs a copy of costs one set of extents instead of one full copy per
    /// destination. A copy-on-write clone is also the only sharing that is safe here:
    /// these staged libraries are rewritten in place later (`install_name_tool`), so
    /// hard links would corrupt the Cargo artifact they were linked to.
    ///
    /// # Errors
    /// Returns an error when the destination cannot be created or a library cannot be copied.
    pub async fn stage(&self, destination: &Path) -> eyre::Result<()> {
        smol::fs::create_dir_all(destination).await?;
        // A resolved source can already live inside the destination — the
        // profile-root dylib a nightly emits — so the staged-copy cleanup must
        // leave sources alone and the copy must not rewrite a library over
        // itself.
        let sources: Vec<PathBuf> = self.iter().map(|path| (*path).to_path_buf()).collect();
        Self::remove_staged_except(destination, &self.triple, &sources).await?;
        for source in &sources {
            let file_name = source.file_name().ok_or_else(|| {
                eyre::eyre!(
                    "Dynamic library path has no file name: {}",
                    source.display()
                )
            })?;
            let staged = destination.join(file_name);
            if *source == staged {
                continue;
            }
            crate::utils::copy_file(source, &staged)
                .await
                .wrap_err_with(|| {
                    format!(
                        "Failed to stage {} to {}",
                        source.display(),
                        staged.display()
                    )
                })?;
        }
        Ok(())
    }

    /// Remove shared-runtime libraries left by an earlier development build.
    ///
    /// # Errors
    /// Returns an error when the destination cannot be read or a matching library cannot be removed.
    pub async fn remove_staged(destination: &Path, triple: &Triple) -> eyre::Result<()> {
        Self::remove_staged_except(destination, triple, &[]).await
    }

    /// `keep` holds library paths that must survive: when a resolved source
    /// already lives in `destination`, deleting it would remove the very
    /// library being staged.
    async fn remove_staged_except(
        destination: &Path,
        triple: &Triple,
        keep: &[PathBuf],
    ) -> eyre::Result<()> {
        if !destination.is_dir() {
            return Ok(());
        }

        let waterui = dynamic_library_file_name("waterui_dylib", triple);
        let (standard_library_prefix, extension) =
            if triple.operating_system == OperatingSystem::Windows {
                ("std-", "dll")
            } else {
                ("libstd-", lib_extension_for_triple(triple))
            };
        let mut entries = smol::fs::read_dir(destination).await?;
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            if keep.contains(&entry.path()) {
                continue;
            }
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name == waterui
                || (file_name.starts_with(standard_library_prefix)
                    && entry.path().extension().and_then(|value| value.to_str()) == Some(extension))
            {
                smol::fs::remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }
}

fn dynamic_library_file_name(crate_name: &str, triple: &Triple) -> String {
    if triple.operating_system == OperatingSystem::Windows {
        format!("{crate_name}.dll")
    } else {
        format!("lib{crate_name}.{}", lib_extension_for_triple(triple))
    }
}

/// Find the dynamic standard library a directory holds for `triple`.
///
/// A missing directory or an empty match set is `NotFound`; several
/// candidates is an error — the caller cannot tell which `libstd` the build
/// actually linked.
fn resolve_rust_standard_library_in(libdir: &Path, triple: &Triple) -> std::io::Result<PathBuf> {
    let (prefix, extension) = if triple.operating_system == OperatingSystem::Windows {
        ("std-", "dll")
    } else {
        ("libstd-", lib_extension_for_triple(triple))
    };
    let entries = match std::fs::read_dir(libdir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} does not exist", libdir.display()),
            ));
        }
        Err(error) => return Err(error),
    };
    let mut matches = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(prefix)
                        && path.extension().and_then(|extension| extension.to_str())
                            == Some(extension)
                })
        })
        .collect::<Vec<_>>();
    matches.sort_unstable();
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Rust target libdir {} contains no dynamic standard library for {triple}",
                libdir.display()
            ),
        )),
        _ => Err(std::io::Error::other(format!(
            "Rust target libdir {} contains multiple dynamic standard libraries for {triple}: {}",
            libdir.display(),
            matches
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Represents a Rust build for a specific target triple.
#[derive(Debug, Clone)]
pub struct RustBuild {
    path: PathBuf,
    triple: Triple,
    project: Option<Project>,
    /// Explicit Cargo target directory for cross-project artifact reuse.
    target_dir: Option<PathBuf>,
    /// Optional path to sccache for compilation caching.
    sccache_path: Option<PathBuf>,
    /// Cargo features to enable.
    features: Vec<String>,
    /// Override the final crate type built by `cargo rustc`.
    crate_type_override: Option<String>,
    /// Extra rustc flags to append via `RUSTFLAGS`.
    rustc_flags: Vec<String>,
    /// Rustc flags that apply to the final crate only, via `cargo rustc -- <flags>`.
    ///
    /// `RUSTFLAGS` is hashed into every dependency unit's fingerprint, so a flag that
    /// only matters when linking the final artifact — an `-rpath` link argument, say —
    /// must not go through [`Self::with_rustc_flag`]: two builds sharing one target
    /// directory that disagree about `RUSTFLAGS` invalidate each other's entire
    /// dependency graph. Trailing `cargo rustc` arguments reach only the selected
    /// target's own compilation and leave dependency fingerprints alone.
    final_rustc_args: Vec<String>,
    /// rustup toolchain name (a nightly) when this build compiles the standard
    /// library from source via `-Zbuild-std`.
    ///
    /// Cargo only ever emits the `rlib` half of a source-built `std`, so a
    /// shared-runtime build on a target whose prebuilt `libstd` is unusable —
    /// Android's is 4 KB-aligned, which 16 KB-page devices reject — runs Cargo
    /// under the `water` rustc wrapper, which adds the `dylib` crate type to
    /// the `std` unit and hands the produced `.so` to every dependent.
    build_std_toolchain: Option<String>,
    /// Extra environment variables to set for the cargo build process.
    envs: Vec<(String, OsString)>,
    /// Sink compile progress is reported to while cargo runs.
    progress: Option<BuildProgress>,
}

/// The optimization/debug-info trade-off a Cargo build selects.
///
/// The variants are realized on top of the workspace's declared `dev` and
/// `release` profiles through `CARGO_PROFILE_*` overrides, so they work on
/// user projects and generated crates alike without manifest changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BuildProfile {
    /// The `dev` profile as declared: unoptimized, with debug info.
    #[default]
    Debug,
    /// The `dev` profile lifted to a light optimization level with full debug
    /// info — the `water run` default for self-drawn backends, whose
    /// per-frame cost sits in rendering dependencies rather than in app code.
    Optimized,
    /// The `release` profile at full speed optimization, without debug info.
    Release,
    /// The `release` profile at full speed optimization, with debug info and
    /// symbols kept so a profiler can symbolicate the recording.
    Profiling,
}

impl BuildProfile {
    /// Whether the build uses Cargo's `release` profile — artifacts land in
    /// the `release/` profile directory and `cargo` gets `--release`.
    #[must_use]
    pub const fn is_release(self) -> bool {
        matches!(self, Self::Release | Self::Profiling)
    }

    /// Whether the profile keeps the development-run shape: the `include_web!`
    /// dev server may serve mounts and the artifact packages as debuggable.
    #[must_use]
    pub const fn is_development(self) -> bool {
        !self.is_release()
    }

    /// `CARGO_PROFILE_*` overrides realizing this profile on the workspace's
    /// declared `dev`/`release` profiles.
    ///
    /// These compose with `profile.*.package."*"` overrides a manifest may
    /// declare: the env sets the profile's base value, so generated crates —
    /// whose `dev` profile already lifts dependencies to `opt-level 2` — keep
    /// that dependency optimization while the base rises to cover the root
    /// crate and the per-unit debug-assertion switches the override table
    /// does not mention.
    fn development_envs(self) -> Vec<(String, OsString)> {
        let entries: &[(&str, &str)] = match self {
            Self::Debug => &[],
            Self::Optimized => &[
                ("CARGO_PROFILE_DEV_OPT_LEVEL", "1"),
                ("CARGO_PROFILE_DEV_DEBUG", "true"),
                ("CARGO_PROFILE_DEV_DEBUG_ASSERTIONS", "false"),
                ("CARGO_PROFILE_DEV_OVERFLOW_CHECKS", "false"),
            ],
            Self::Release => &[("CARGO_PROFILE_RELEASE_OPT_LEVEL", "3")],
            Self::Profiling => &[
                ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "3"),
                ("CARGO_PROFILE_RELEASE_DEBUG", "true"),
                ("CARGO_PROFILE_RELEASE_STRIP", "none"),
            ],
        };
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(*value)))
            .collect()
    }
}

/// Options for building Rust libraries.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    profile: BuildProfile,
    output_dir: Option<std::path::PathBuf>,
    /// Optional path to sccache for compilation caching.
    sccache_path: Option<std::path::PathBuf>,
    /// Optional target triple override.
    target_triple: Option<Triple>,
    /// Rust runtime linkage used by the final native application.
    linkage: RustLinkage,
    /// Whether the built app will `dlopen` `WaterUI` modules — a preview
    /// support app — and therefore must package the shared Rust runtime
    /// instead of linking it in, even on a platform that otherwise forces
    /// static linkage.
    dynamic_module_loading: bool,
    /// Whether `include_web!` mounts are dev-server-served and skipped when
    /// the build stages assets (Hydrolysis stages at build time).
    dev_server: bool,
    /// `CARGO_PROFILE_*` overrides applied to the cargo invocation.
    cargo_envs: Vec<(String, OsString)>,
    /// Sink compile progress is reported to while cargo runs.
    progress: Option<BuildProgress>,
}

impl BuildOptions {
    /// Create options for a development build that uses the shared Rust runtime.
    ///
    /// Development runs want wall-clock speed: `Release` and `Profiling` force
    /// `opt-level 3` rather than the size-optimized `opt-level "z"` the
    /// packaging profile declares, and `Optimized`/`Profiling`/`Release` all
    /// carry `CARGO_PROFILE_*` overrides the cargo invocation applies.
    #[must_use]
    pub fn development(profile: BuildProfile) -> Self {
        Self {
            profile,
            output_dir: None,
            sccache_path: None,
            target_triple: None,
            linkage: RustLinkage::SharedRuntime,
            dynamic_module_loading: false,
            dev_server: false,
            cargo_envs: profile.development_envs(),
            progress: None,
        }
    }

    /// Link the Rust runtime in, whatever the caller asked for.
    ///
    /// A platform whose loader cannot accept the toolchain's prebuilt runtime
    /// says so here rather than at the link step, so that the target directory
    /// and the staged libraries agree with what is actually built.
    #[must_use]
    pub const fn with_static_runtime(mut self) -> Self {
        self.linkage = RustLinkage::Static;
        self
    }

    /// Create options for a self-contained package build.
    ///
    /// A packaged artifact builds under the profile the workspace declares —
    /// no `CARGO_PROFILE_*` overrides: the release profile's size tuning
    /// (`opt-level "z"`, symbol stripping) is the shipped configuration.
    #[must_use]
    pub const fn packaging(profile: BuildProfile) -> Self {
        Self {
            profile,
            output_dir: None,
            sccache_path: None,
            target_triple: None,
            linkage: RustLinkage::Static,
            dynamic_module_loading: false,
            dev_server: false,
            cargo_envs: Vec::new(),
            progress: None,
        }
    }

    /// Whether the build uses Cargo's `release` profile.
    #[must_use]
    pub const fn is_release(&self) -> bool {
        self.profile.is_release()
    }

    /// The selected build profile.
    #[must_use]
    pub const fn profile(&self) -> BuildProfile {
        self.profile
    }

    /// `CARGO_PROFILE_*` overrides the cargo invocation applies.
    #[must_use]
    pub fn cargo_envs(&self) -> &[(String, OsString)] {
        &self.cargo_envs
    }

    /// Mark web mounts as dev-server-served for asset staging this build does.
    #[must_use]
    pub const fn with_dev_server(mut self, dev_server: bool) -> Self {
        self.dev_server = dev_server;
        self
    }

    /// Whether web mounts are dev-server-served and skipped during staging.
    #[must_use]
    pub const fn uses_dev_server(&self) -> bool {
        self.dev_server
    }

    /// Get the output directory, if specified
    #[must_use]
    pub fn output_dir(&self) -> Option<&std::path::Path> {
        self.output_dir.as_deref()
    }

    /// Set the output directory where built libraries should be copied
    #[must_use]
    pub fn with_output_dir(mut self, output_dir: impl Into<std::path::PathBuf>) -> Self {
        self.output_dir = Some(output_dir.into());
        self
    }

    /// Get the sccache path, if configured
    #[must_use]
    pub fn sccache_path(&self) -> Option<&std::path::Path> {
        self.sccache_path.as_deref()
    }

    /// Set the sccache path for compilation caching.
    ///
    /// When set, `RUSTC_WRAPPER` will be configured to use sccache,
    /// which can significantly improve build times by caching compiled artifacts.
    #[must_use]
    pub fn with_sccache(mut self, sccache_path: impl Into<std::path::PathBuf>) -> Self {
        self.sccache_path = Some(sccache_path.into());
        self
    }

    /// Get the explicit target triple override, if configured.
    #[must_use]
    pub const fn target_triple(&self) -> Option<&Triple> {
        self.target_triple.as_ref()
    }

    /// Override the target triple used for compilation.
    #[must_use]
    pub fn with_target_triple(mut self, target_triple: Triple) -> Self {
        self.target_triple = Some(target_triple);
        self
    }

    /// Get the selected Rust runtime linkage.
    #[must_use]
    pub const fn linkage(&self) -> RustLinkage {
        self.linkage
    }

    /// Mark the built app as a host for `dlopen`'d `WaterUI` modules.
    ///
    /// A preview support app resolves a pushed module's framework symbols
    /// against the runtime it already has open, so the shared runtime has to
    /// ship in the package rather than be linked into the app alone.
    #[must_use]
    pub const fn with_dynamic_module_loading(mut self) -> Self {
        self.dynamic_module_loading = true;
        self
    }

    /// Whether the built app hosts dynamically loaded `WaterUI` modules.
    #[must_use]
    pub const fn loads_dynamic_modules(&self) -> bool {
        self.dynamic_module_loading
    }

    /// Attach a compile-progress sink every cargo invocation this build
    /// performs reports to.
    #[must_use]
    pub fn with_progress(mut self, progress: BuildProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    /// The compile-progress sink, when one is attached.
    #[must_use]
    pub const fn progress(&self) -> Option<&BuildProgress> {
        self.progress.as_ref()
    }
}

/// Errors that can occur during the Rust build process.
#[derive(Debug, thiserror::Error)]
pub enum RustBuildError {
    /// Failed to execute cargo build.
    #[error("Failed to execute cargo build: {0}")]
    FailToExecuteCargoBuild(std::io::Error),

    /// Cargo executed but failed to build the Rust library.
    #[error("Failed to build Rust library: {0}")]
    FailToBuildRustLibrary(std::io::Error),
}

/// Cargo's compile-phase progress: one event per status line cargo writes to
/// stderr.
///
/// A cold build reports nothing to a captured pipe for its whole duration, so
/// `water run` and `water build` attach a [`BuildProgress`] sink that keeps
/// the compile visibly alive on every terminal. Events are parsed from
/// cargo's own output, never generated by a timer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileEvent {
    /// A `name vversion` status line: one crate unit moved through cargo's
    /// pipeline. `phase` is cargo's status word — `Compiling`, `Checking`,
    /// `Fresh`, `Downloading`, `Downloaded` or `Doc-tests`.
    Unit {
        /// Cargo's status word.
        phase: &'static str,
        /// The crate the status line names.
        name: String,
        /// The crate's version, when the status line carries one.
        version: Option<String>,
    },
    /// `Finished ...` — cargo's closing status line.
    Finished(String),
    /// Any other line — index and lock status, warnings, diagnostics,
    /// build-script output.
    Line(String),
}

/// The sink a cargo build reports its [`CompileEvent`]s into.
///
/// The terminal attaches one per build; events arrive on the task draining
/// cargo's stderr, so a sink must stay cheap.
#[derive(Clone)]
pub struct BuildProgress {
    report: std::sync::Arc<dyn Fn(CompileEvent) + Send + Sync>,
    /// Whether the sink renders every line live. When it does, a build
    /// failure report can tail the captured output instead of re-dumping what
    /// the user already watched scroll by.
    shows_all_lines: bool,
}

impl std::fmt::Debug for BuildProgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BuildProgress(..)")
    }
}

impl BuildProgress {
    /// A sink that renders each event through `report`.
    #[must_use]
    pub fn new(report: impl Fn(CompileEvent) + Send + Sync + 'static) -> Self {
        Self {
            report: std::sync::Arc::new(report),
            shows_all_lines: false,
        }
    }

    /// Mark the sink as rendering every line live, including
    /// [`CompileEvent::Line`] diagnostics.
    #[must_use]
    pub const fn showing_all_lines(mut self) -> Self {
        self.shows_all_lines = true;
        self
    }

    /// Whether the sink renders every line live.
    #[must_use]
    pub const fn shows_all_lines(&self) -> bool {
        self.shows_all_lines
    }

    fn report(&self, event: CompileEvent) {
        (self.report)(event);
    }
}

/// Cargo status words whose line names one crate unit: `phase name vversion`.
const CARGO_UNIT_PHASES: &[&str] = &[
    "Compiling",
    "Checking",
    "Fresh",
    "Downloading",
    "Downloaded",
    "Doc-tests",
];

/// Classify one line of cargo's stderr into a [`CompileEvent`].
///
/// Cargo emits ANSI-colored status lines whenever color is forced — by the
/// `CARGO_TERM_COLOR` this module sets for terminal output, or by the user's
/// own `[term] color` configuration — so the line is classified on its
/// stripped text. Text-carrying events keep the raw line: an interactive sink
/// renders cargo's colors, and the piped and JSON renderers strip on emit.
fn classify_compile_line(line: &str) -> CompileEvent {
    let raw = line.trim();
    let stripped = console::strip_ansi_codes(raw);
    let text = stripped.trim();
    for phase in CARGO_UNIT_PHASES {
        let Some(rest) = text
            .strip_prefix(phase)
            .and_then(|rest| rest.strip_prefix(' '))
        else {
            continue;
        };
        // A unit line names `name vversion`; `Downloaded 12 crates` and
        // `Doc-tests foo` are status text, not a unit.
        let Some((name, version)) = rest.split_once(" v") else {
            return CompileEvent::Line(raw.to_owned());
        };
        let version = version.split([' ', '(']).next().unwrap_or_default();
        return CompileEvent::Unit {
            phase,
            name: name.to_owned(),
            version: (!version.is_empty()).then(|| version.to_owned()),
        };
    }
    if text.starts_with("Finished ") {
        return CompileEvent::Finished(raw.to_owned());
    }
    CompileEvent::Line(raw.to_owned())
}

/// Spawn a configured command with piped stdio, drain both streams to their
/// ends, and report cargo's stderr status lines to `progress`.
///
/// The returned [`std::process::Output`] is exactly what `output()` produces:
/// pipes are always drained and collected in full, so failure reporting and
/// retry detection see the same captured text whether or not a sink is
/// attached. When no sink is attached and the CLI's output passthrough is
/// enabled, raw stderr chunks echo to the terminal as they arrive — the
/// historical `Stdio::inherit` behavior. Stdout is collected silently: a
/// `--message-format=json` caller parses it as a protocol stream, so it is
/// never mirrored.
pub(crate) async fn command_output_with_progress(
    command: &mut Command,
    progress: Option<BuildProgress>,
) -> io::Result<std::process::Output> {
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout_pipe = child.stdout.take().expect("stdout is piped");
    let stderr_pipe = child.stderr.take().expect("stderr is piped");

    // Raw chunk echo reproduces `Stdio::inherit` for a build carrying no
    // progress sink; a sink renders the parsed events itself.
    let echo = progress.is_none() && std_output_enabled();
    // The drains run as their own tasks: inlined into this future their read
    // buffers alone would push it past clippy's `large_futures` threshold.
    let stdout_task = smol::spawn(drain_pipe(stdout_pipe));
    let stderr_task = smol::spawn(drain_cargo_stderr(stderr_pipe, progress, echo));
    let status = child.status().await?;
    let stdout = stdout_task.await?;
    let stderr = stderr_task.await?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Drain a piped child stream to EOF, collecting every byte.
async fn drain_pipe(mut reader: impl smol::io::AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut collected = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        collected.extend_from_slice(&chunk[..read]);
    }
    Ok(collected)
}

/// Drain cargo's piped stderr: collect every byte, echo raw chunks when
/// passthrough is enabled, and report each completed line's [`CompileEvent`]
/// to `progress` as it arrives.
async fn drain_cargo_stderr(
    mut reader: impl smol::io::AsyncRead + Unpin,
    progress: Option<BuildProgress>,
    echo: bool,
) -> io::Result<Vec<u8>> {
    let mut collected = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        collected.extend_from_slice(&chunk[..read]);
        if echo {
            let _ = io::stderr().write_all(&chunk[..read]);
            let _ = io::stderr().flush();
        }
        if let Some(sink) = &progress {
            pending.extend_from_slice(&chunk[..read]);
            // A line feed is never a UTF-8 continuation byte, so scanning raw
            // bytes for line boundaries and decoding only complete lines
            // cannot corrupt a multibyte character straddling a chunk.
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=newline).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end();
                if !line.trim().is_empty() {
                    sink.report(classify_compile_line(line));
                }
            }
        }
    }
    if let Some(sink) = &progress {
        let tail = String::from_utf8_lossy(&pending);
        let tail = tail.trim_end();
        if !tail.trim().is_empty() {
            sink.report(classify_compile_line(tail));
        }
    }
    Ok(collected)
}

impl RustBuild {
    /// Create a new rust build for the given path and target triple.
    pub fn new(path: impl AsRef<Path>, triple: Triple) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            triple,
            project: None,
            target_dir: None,
            sccache_path: None,
            features: Vec::new(),
            crate_type_override: None,
            rustc_flags: Vec::new(),
            final_rustc_args: Vec::new(),
            build_std_toolchain: None,
            envs: Vec::new(),
            progress: None,
        }
    }

    pub(crate) fn with_project(mut self, project: &Project) -> Self {
        self.project = Some(project.clone());
        self
    }

    /// Use an explicit Cargo target directory.
    #[must_use]
    pub fn with_target_dir(mut self, target_dir: impl Into<PathBuf>) -> Self {
        self.target_dir = Some(target_dir.into());
        self
    }

    /// Set the sccache path for compilation caching.
    ///
    /// When set, `RUSTC_WRAPPER` will be configured to use sccache,
    /// which can significantly improve incremental build times.
    #[must_use]
    pub fn with_sccache(mut self, sccache_path: PathBuf) -> Self {
        self.sccache_path = Some(sccache_path);
        self
    }

    /// Add a Cargo feature to enable during the build.
    ///
    /// Features are passed to cargo via `--features`.
    #[must_use]
    pub fn with_feature(mut self, feature: impl Into<String>) -> Self {
        self.features.push(feature.into());
        self
    }

    /// Add multiple Cargo features to enable during the build.
    #[must_use]
    pub fn with_features(mut self, features: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.features.extend(features.into_iter().map(Into::into));
        self
    }

    /// Cargo features this build passes via `--features`.
    #[must_use]
    pub fn features(&self) -> &[String] {
        &self.features
    }

    /// Add a rustc flag to the build via `RUSTFLAGS`.
    #[must_use]
    pub fn with_rustc_flag(mut self, flag: impl Into<String>) -> Self {
        self.rustc_flags.push(flag.into());
        self
    }

    /// Add a rustc flag that applies to the final crate only.
    ///
    /// The flag is passed as a trailing `cargo rustc` argument instead of through
    /// `RUSTFLAGS`, so dependency unit fingerprints stay identical across builds that
    /// differ only in how their final artifact links. See the field documentation on
    /// `final_rustc_args` for why link arguments must take this route.
    #[must_use]
    pub fn with_final_rustc_arg(mut self, flag: impl Into<String>) -> Self {
        self.final_rustc_args.push(flag.into());
        self
    }

    /// Build the Rust standard library from source with `-Zbuild-std` on the
    /// named toolchain (a nightly with `rust-src`), sharing one `libstd`
    /// dylib across the graph.
    ///
    /// The build runs Cargo under the `water` rustc wrapper
    /// ([`crate::rustc_wrapper`]): Cargo strips `dylib` from `std`'s crate
    /// types under `-Zbuild-std`, and the wrapper restores it so the produced
    /// `libstd-*.so` carries the same strict version hash as the rlib every
    /// dependent is compiled against. The wrapper also publishes the dylib
    /// into the profile's `deps/` directory, where
    /// [`RustDynamicLibraries::resolve`] finds it before the toolchain's
    /// prebuilt copy.
    #[must_use]
    pub fn with_build_std(mut self, toolchain: impl Into<String>) -> Self {
        self.build_std_toolchain = Some(toolchain.into());
        self
    }

    /// Prefer dynamic Rust dependencies and emit loader search paths for them.
    #[must_use]
    pub fn with_preferred_dynamic_linking(self) -> Self {
        self.with_rustc_flag("-Cprefer-dynamic")
            .with_rustc_flag("-Crpath")
    }

    /// Configure this build for the selected Rust runtime linkage.
    ///
    /// A shared-runtime development build enables the project's `dev` feature (which
    /// resolves the shared `waterui-dylib` runtime), prefers dynamic linking, and —
    /// when the platform's loader needs one — embeds a loader search path into the
    /// final artifact only. A static packaging build needs none of this.
    #[must_use]
    pub fn with_linkage(
        self,
        linkage: RustLinkage,
        development_feature: &str,
        loader_search_path: Option<&str>,
    ) -> Self {
        if linkage == RustLinkage::Static {
            return self;
        }
        let build = self
            .with_feature(development_feature)
            .with_preferred_dynamic_linking();
        match loader_search_path {
            Some(path) => build.with_final_rustc_arg(format!("-Clink-arg=-Wl,-rpath,{path}")),
            None => build,
        }
    }

    /// Override the library crate type passed to `rustc`.
    #[must_use]
    pub fn with_crate_type_override(mut self, crate_type: impl Into<String>) -> Self {
        self.crate_type_override = Some(crate_type.into());
        self
    }

    /// Add an environment variable for the cargo build process.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Add multiple environment variables for the cargo build process.
    #[must_use]
    pub fn with_envs(mut self, envs: impl IntoIterator<Item = (String, OsString)>) -> Self {
        self.envs.extend(envs);
        self
    }

    /// Attach a compile-progress sink the cargo invocation reports to.
    ///
    /// Each [`CompileEvent`] is parsed from cargo's own stderr stream, so the
    /// report is driven by build output rather than a timer.
    #[must_use]
    pub fn with_progress(mut self, progress: BuildProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Get the target triple for this build.
    #[must_use]
    pub const fn triple(&self) -> &Triple {
        &self.triple
    }

    /// Build rust library in development mode.
    ///
    /// Will produce debug symbols and less optimizations for faster builds.
    ///
    /// Return the path to the built library.
    ///
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If there was an error building the Rust library.
    pub async fn dev_build(&self) -> Result<PathBuf, RustBuildError> {
        self.build_lib(false).await
    }

    /// Build rust library in release mode.
    ///
    /// Return the directory path containing the built library.
    ///
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If there was an error building the Rust library.
    pub async fn release_build(&self) -> Result<PathBuf, RustBuildError> {
        self.build_lib(true).await
    }

    /// Build a library with the specified crate type.
    ///
    /// Return the directory path containing the built library.
    ///
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If there was an error building the Rust library.
    pub async fn build_lib(&self, release: bool) -> Result<PathBuf, RustBuildError> {
        self.build_inner(release, CargoTarget::Lib).await
    }

    /// Build a dynamic library (cdylib) and return the full path to the dylib file.
    ///
    /// This is a convenience method that builds the library and computes the full
    /// path to the resulting dylib file based on the crate name and target triple.
    ///
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If the library was not found after building.
    pub async fn build_dylib(
        &self,
        crate_name: &str,
        release: bool,
    ) -> Result<PathBuf, RustBuildError> {
        let lib_dir = self.build_inner(release, CargoTarget::Lib).await?;

        let lib_name = crate_name.replace('-', "_");
        let ext = lib_extension_for_triple(&self.triple);
        let dylib_path = lib_dir.join(format!("lib{lib_name}.{ext}"));

        if !dylib_path.exists() {
            return Err(RustBuildError::FailToBuildRustLibrary(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Dynamic library not found at {} after cargo build",
                    dylib_path.display()
                ),
            )));
        }

        Ok(dylib_path)
    }

    /// Builds one named binary and returns its full output path.
    ///
    /// # Errors
    ///
    /// Returns an error when Cargo fails or the expected binary is missing.
    pub async fn build_binary(
        &self,
        binary_name: &str,
        release: bool,
    ) -> Result<PathBuf, RustBuildError> {
        let output_dir = self
            .build_inner(release, CargoTarget::Binary(binary_name))
            .await?;
        let binary_file_name = if self.triple.operating_system == OperatingSystem::Windows {
            format!("{binary_name}.exe")
        } else {
            binary_name.to_string()
        };
        let binary_path = output_dir.join(binary_file_name);
        if !binary_path.is_file() {
            return Err(RustBuildError::FailToBuildRustLibrary(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Binary not found at {} after cargo build",
                    binary_path.display()
                ),
            )));
        }
        Ok(binary_path)
    }

    /// Compute the expected dylib output path without building.
    ///
    /// This uses `cargo metadata` to resolve the target directory to avoid assuming
    /// a fixed `target/` path.
    ///
    /// # Errors
    /// Returns an error if Cargo metadata cannot be read.
    pub async fn dylib_path(
        &self,
        crate_name: &str,
        release: bool,
    ) -> Result<PathBuf, RustBuildError> {
        let lib_dir = self.lib_output_dir(release).await?;
        let lib_name = crate_name.replace('-', "_");
        let ext = lib_extension_for_triple(&self.triple);
        Ok(lib_dir.join(format!("lib{lib_name}.{ext}")))
    }

    /// Return target directory path
    async fn build_inner(
        &self,
        release: bool,
        cargo_target: CargoTarget<'_>,
    ) -> Result<PathBuf, RustBuildError> {
        let mut output = self.cargo_build_output(release, cargo_target).await?;

        if !output.status.success() {
            let mut combined = combined_build_output(&output);

            // Handle stale CMake generator caches (e.g. Unix Makefiles vs Ninja)
            // by cleaning crate-local CMake build dirs and retrying once.
            if should_retry_after_cmake_generator_mismatch(&combined)
                && self.clean_stale_cmake_build_dirs().await?
            {
                output = self.cargo_build_output(release, cargo_target).await?;
                combined = combined_build_output(&output);
            }

            if !output.status.success() && should_auto_install_meson(&combined) {
                match ensure_meson_installed_for_build().await {
                    Ok(()) => {
                        output = self.cargo_build_output(release, cargo_target).await?;
                    }
                    Err(install_err) => {
                        return Err(RustBuildError::FailToBuildRustLibrary(
                            std::io::Error::other(format!(
                                "Cargo build failed and meson appears missing.\n\
Automatic meson installation failed: {install_err}\n\n{}",
                                self.failure_report(&combined)
                            )),
                        ));
                    }
                }
            }
        }

        if !output.status.success() {
            let combined = combined_build_output(&output);
            return Err(RustBuildError::FailToBuildRustLibrary(
                std::io::Error::other(format!(
                    "Cargo build failed:\n{}",
                    self.failure_report(&combined)
                )),
            ));
        }

        self.lib_output_dir(release).await
    }

    /// The text a build failure report embeds: the whole captured output, or
    /// only its tail when the attached sink already rendered every line live.
    fn failure_report(&self, combined: &str) -> String {
        if self
            .progress
            .as_ref()
            .is_some_and(BuildProgress::shows_all_lines)
        {
            output_tail(combined)
        } else {
            combined.to_owned()
        }
    }

    async fn clean_stale_cmake_build_dirs(&self) -> Result<bool, RustBuildError> {
        let target_dir = self.target_directory().await?;
        let triple = self.triple.to_string();

        let removed = unblock(move || {
            let mut removed = 0usize;
            removed +=
                remove_cmake_build_dirs_in(&target_dir.join(&triple).join("debug").join("build"))?;
            removed += remove_cmake_build_dirs_in(
                &target_dir.join(&triple).join("release").join("build"),
            )?;
            Ok::<usize, std::io::Error>(removed)
        })
        .await
        .map_err(|error| {
            RustBuildError::FailToBuildRustLibrary(std::io::Error::other(format!(
                "Failed to clean stale CMake cache: {error}"
            )))
        })?;

        Ok(removed > 0)
    }

    async fn cargo_build_output(
        &self,
        release: bool,
        cargo_target: CargoTarget<'_>,
    ) -> Result<std::process::Output, RustBuildError> {
        let framework = self.project.as_ref().and_then(|project| {
            project
                .manifest()
                .framework
                .as_ref()
                .map(|framework| (project, framework))
        });
        if let Some((project, framework)) = framework {
            framework
                .prepare_build(project, &self.path, &self.features)
                .await
                .map_err(|error| {
                    RustBuildError::FailToBuildRustLibrary(std::io::Error::other(error.to_string()))
                })?;
        }
        let crate_type_override = if cargo_target.accepts_crate_type_override() {
            self.crate_type_override.as_deref()
        } else {
            None
        };
        let mut cmd = Command::new("cargo");
        let cargo_subcommand = if crate_type_override.is_some() || !self.final_rustc_args.is_empty()
        {
            "rustc"
        } else {
            "build"
        };
        let mut cmd = cmd.arg(cargo_subcommand);
        if self.build_std_toolchain.is_some() {
            // `-Zbuild-std-features` replaces Cargo's default std feature set
            // — `panic-unwind,backtrace,default` (cargo's `standard_lib.rs`)
            // — so all three are listed back explicitly; `default` keeps each
            // std-workspace crate's own defaults, notably `compiler_builtins`'s
            // `arch` routines. `compiler-builtins-c` then links the NDK's
            // prebuilt compiler-rt archive — on aarch64 that provides the LSE
            // outline-atomics helpers (`__aarch64_ldadd4_acq_rel` & friends)
            // that NDK-compiled C objects reference, which otherwise stay
            // undefined and make `dlopen` reject the libraries.
            cmd = cmd.arg("-Zbuild-std=std,panic_abort");
            cmd =
                cmd.arg("-Zbuild-std-features=panic-unwind,backtrace,default,compiler-builtins-c");
        }
        let mut cmd = cmd
            .args(cargo_target.cargo_args())
            .args(["--target", self.triple.to_string().as_str()])
            .current_dir(&self.path);
        if framework.is_some() {
            cmd = cmd.arg("--locked");
        }

        if let Some(target_dir) = &self.target_dir {
            cmd = cmd.arg("--target-dir").arg(target_dir);
        }

        // Apply extra environment variables (caller-provided values override defaults).
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }

        if !self.rustc_flags.is_empty() {
            let mut rustflags = std::env::var_os("RUSTFLAGS").unwrap_or_default();
            if !rustflags.is_empty() {
                rustflags.push(" ");
            }
            rustflags.push(self.rustc_flags.join(" "));
            cmd = cmd.env("RUSTFLAGS", rustflags);
        }

        configure_generated_crate_compilation(cmd);

        // Use sccache as rustc wrapper if configured
        if let Some(sccache_path) = &self.sccache_path {
            crate::toolchain::sccache::configure_compilation_cache(cmd, sccache_path);
        }

        // A `-Zbuild-std` build runs the `water` binary itself as
        // `RUSTC_WRAPPER`, chained in front of sccache when one is configured,
        // so the wrapper can add the `dylib` crate type Cargo strips from the
        // `std` unit and publish the produced `libstd-*.so` into `deps/`.
        // This must come after the sccache block above to win `RUSTC_WRAPPER`.
        if self.build_std_toolchain.is_some() {
            cmd = self.with_build_std_envs(cmd, release).await?;
        }

        // Set target-scoped bindgen clang args for simulator builds.
        //
        // Using the global `BINDGEN_EXTRA_CLANG_ARGS` leaks the simulator SDK into
        // host-side build scripts (for example `coreaudio-sys`), which then try to
        // parse host frameworks against the simulator SDK and fail. Bindgen supports
        // target-qualified env vars, so scope the override to the actual Cargo target.
        if self.triple.environment == Environment::Sim
            && let Some(clang_args) = self.bindgen_clang_args_for_simulator().await
        {
            let bindgen_target_key = format!(
                "BINDGEN_EXTRA_CLANG_ARGS_{}",
                self.triple.to_string().replace('-', "_")
            );
            cmd = cmd.env(bindgen_target_key, clang_args);
        }

        if release {
            cmd = cmd.arg("--release");
        }

        // Add cargo features if specified
        if !self.features.is_empty() {
            cmd = cmd.args(["--features", &self.features.join(",")]);
        }

        if crate_type_override.is_some() || !self.final_rustc_args.is_empty() {
            cmd = cmd.arg("--");
            if let Some(crate_type) = crate_type_override {
                cmd = cmd.arg("--crate-type").arg(crate_type);
            }
            cmd = cmd.args(&self.final_rustc_args);
        }

        // Piped stdio strips rustc diagnostics of their colors; when the
        // terminal renders them — through the progress sink or the raw
        // passthrough echo — restore cargo's coloring unless the caller
        // configured it explicitly.
        if std_output_enabled()
            && std::env::var_os("CARGO_TERM_COLOR").is_none()
            && !self.envs.iter().any(|(key, _)| key == "CARGO_TERM_COLOR")
        {
            cmd.env("CARGO_TERM_COLOR", "always");
        }

        command_output_with_progress(cmd, self.progress.clone())
            .await
            .map_err(RustBuildError::FailToExecuteCargoBuild)
    }

    /// Point a `-Zbuild-std` cargo invocation at the nightly toolchain and at
    /// this binary as `RUSTC_WRAPPER`, chained in front of sccache when one is
    /// configured.
    async fn with_build_std_envs<'a>(
        &self,
        cmd: &'a mut Command,
        release: bool,
    ) -> Result<&'a mut Command, RustBuildError> {
        let Some(toolchain) = &self.build_std_toolchain else {
            return Ok(cmd);
        };
        let publish_dir = self.lib_output_dir(release).await?.join("deps");
        let cmd = cmd
            .env("RUSTUP_TOOLCHAIN", toolchain)
            .env(
                "RUSTC_WRAPPER",
                crate::toolchain::Host::current_exe()
                    .map_err(RustBuildError::FailToExecuteCargoBuild)?,
            )
            .env(crate::workflows::rustc_wrapper::WRAPPER_MODE_ENV, "1")
            .env(
                crate::workflows::rustc_wrapper::BUILD_STD_TARGET_ENV,
                self.triple.to_string(),
            )
            .env(
                crate::workflows::rustc_wrapper::BUILD_STD_DYLIB_DIR_ENV,
                publish_dir,
            );
        if let Some(sccache_path) = &self.sccache_path {
            cmd.env(
                crate::workflows::rustc_wrapper::WRAPPER_CHAIN_ENV,
                sccache_path,
            );
        }
        // A workspace wrapper replaces `RUSTC_WRAPPER` on workspace-member
        // units — the support app's ffi crate and the generated module crate
        // are exactly the link-emitting members that need the `std` dylib
        // extern. Without it they would link `std` statically while the deps
        // link dynamically: two panic runtimes in one process.
        cmd.env_remove("RUSTC_WORKSPACE_WRAPPER");
        cmd.env_remove("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER");
        Ok(cmd)
    }

    /// Resolve the Cargo library artifact directory for this build target and profile.
    ///
    /// # Errors
    /// Returns an error if Cargo metadata cannot be read for this build target.
    pub async fn lib_output_dir(&self, release: bool) -> Result<PathBuf, RustBuildError> {
        let target_directory = self.target_directory().await?;
        Ok(target_directory
            .join(self.triple.to_string())
            .join(if release { "release" } else { "debug" }))
    }

    async fn target_directory(&self) -> Result<PathBuf, RustBuildError> {
        if let Some(target_dir) = &self.target_dir {
            return Ok(target_dir.clone());
        }

        let build_path = self.path.clone();
        let metadata = unblock(move || {
            cargo_metadata::MetadataCommand::new()
                .no_deps()
                .current_dir(build_path)
                .exec()
                .map_err(|e| {
                    RustBuildError::FailToBuildRustLibrary(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e,
                    ))
                })
        })
        .await?;
        Ok(metadata.target_directory.as_std_path().to_path_buf())
    }

    /// Generate `BINDGEN_EXTRA_CLANG_ARGS` for simulator builds.
    ///
    /// Bindgen has issues with the `*-apple-*-sim` target triples, so we need to
    /// provide explicit clang arguments with a proper target and SDK path.
    async fn bindgen_clang_args_for_simulator(&self) -> Option<String> {
        let (sdk_name, target_os) = match self.triple.operating_system {
            OperatingSystem::IOS(_) => ("iphonesimulator", "ios"),
            OperatingSystem::TvOS(_) => ("appletvsimulator", "tvos"),
            OperatingSystem::WatchOS(_) => ("watchsimulator", "watchos"),
            OperatingSystem::VisionOS(_) => ("xrsimulator", "xros"),
            _ => return None,
        };

        let arch = match self.triple.architecture {
            target_lexicon::Architecture::Aarch64(_) => "arm64",
            target_lexicon::Architecture::X86_64 => "x86_64",
            _ => return None,
        };

        // Get SDK path using xcrun
        let sdk_path = run_command("xcrun", ["--sdk", sdk_name, "--show-sdk-path"])
            .await
            .ok()
            .map(|s| s.trim().to_string())?;

        // Use a reasonable minimum deployment target
        let min_version = if matches!(target_os, "ios" | "tvos") {
            "17.0"
        } else if target_os == "watchos" {
            "10.0"
        } else {
            debug_assert_eq!(
                target_os, "xros",
                "bindgen simulator target_os must be one of ios/tvos/watchos/xros"
            );
            "1.0"
        };

        Some(format!(
            "--target={arch}-apple-{target_os}{min_version}-simulator -isysroot {sdk_path}"
        ))
    }
}

fn combined_build_output(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stderr.is_empty() {
        stdout.to_string()
    } else {
        stderr.to_string()
    }
}

/// Lines a failure report keeps when the terminal already streamed the whole
/// build live — the dump is truncated to this tail.
const FAILURE_TAIL_LINES: usize = 40;

/// The last [`FAILURE_TAIL_LINES`] lines of `text` — what a failure report
/// needs when the terminal already rendered the full stream.
pub(crate) fn output_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= FAILURE_TAIL_LINES {
        return text.to_owned();
    }
    format!(
        "… {} earlier lines already streamed above …\n{}",
        lines.len() - FAILURE_TAIL_LINES,
        lines[lines.len() - FAILURE_TAIL_LINES..].join("\n")
    )
}

fn should_auto_install_meson(build_output: &str) -> bool {
    let lower = build_output.to_ascii_lowercase();
    lower.contains("meson")
        && (lower.contains("not found")
            || lower.contains("no such file")
            || lower.contains("failed to execute")
            || lower.contains("is required"))
}

fn should_retry_after_cmake_generator_mismatch(build_output: &str) -> bool {
    let lower = build_output.to_ascii_lowercase();
    lower.contains("cmake error") && lower.contains("does not match the generator used previously")
}

fn remove_cmake_build_dirs_in(build_root: &Path) -> std::io::Result<usize> {
    if !build_root.exists() {
        return Ok(0);
    }

    let mut removed = 0usize;
    for entry in std::fs::read_dir(build_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let cmake_build_dir = path.join("out").join("build");
        if cmake_build_dir.join("CMakeCache.txt").exists() {
            std::fs::remove_dir_all(cmake_build_dir)?;
            removed += 1;
        }
    }

    Ok(removed)
}

#[cfg(target_os = "macos")]
async fn ensure_meson_installed_for_build() -> Result<(), String> {
    use crate::toolchain::meson::Meson;
    use crate::toolchain::{Installation as _, Toolchain as _, ToolchainError};

    let host = crate::toolchain::Host::current();
    match Meson.check(&host).await {
        Ok(()) => Ok(()),
        Err(ToolchainError::Fixable(installation)) => {
            installation.install(&host).await.map_err(|e| e.to_string())
        }
        Err(ToolchainError::Unfixable(e)) => Err(e.to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_meson_installed_for_build() -> impl std::future::Future<Output = Result<(), String>> {
    std::future::ready(Err(
        "automatic meson installation is only supported on macOS".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use target_lexicon::Triple;
    use tempfile::tempdir;

    use std::ffi::OsString;

    use super::{
        BuildOptions, BuildProfile, CargoTarget, CompileEvent, RustBuild, RustDynamicLibraries,
        RustLinkage, classify_compile_line, dynamic_library_file_name, lib_extension_for_triple,
        resolve_rust_standard_library_in,
    };

    fn triple(value: &str) -> Triple {
        value.parse().expect("test target triple must parse")
    }

    #[test]
    fn crate_type_override_applies_only_to_library_targets() {
        assert!(CargoTarget::Lib.accepts_crate_type_override());
        assert!(!CargoTarget::Binary("waterui-cef-helper").accepts_crate_type_override());
        assert_eq!(CargoTarget::Lib.cargo_args(), ["--lib"]);
        assert_eq!(
            CargoTarget::Binary("waterui-cef-helper").cargo_args(),
            ["--bin", "waterui-cef-helper"]
        );
    }

    #[test]
    fn build_std_envs_wire_the_wrapper_and_clear_workspace_wrappers() {
        use std::ffi::OsStr;

        let dir = tempdir().expect("target dir");
        let toolchain = "nightly-2026-09-09-aarch64-apple-darwin";
        let target_dir = dir.path().join("target");
        let build = RustBuild::new(dir.path(), triple("aarch64-linux-android"))
            .with_build_std(toolchain)
            .with_target_dir(target_dir.clone())
            .with_sccache(std::path::PathBuf::from("/fake/sccache"));
        let mut cmd = smol::process::Command::new("cargo");
        smol::block_on(build.with_build_std_envs(&mut cmd, false)).expect("build-std envs apply");

        let env = |key: &str| -> Option<Option<OsString>> {
            cmd.get_envs()
                .find(|(name, _)| *name == OsStr::new(key))
                .map(|(_, value)| value.map(ToOwned::to_owned))
        };
        assert_eq!(
            env("RUSTUP_TOOLCHAIN"),
            Some(Some(OsString::from(toolchain)))
        );
        assert_eq!(
            env("RUSTC_WRAPPER"),
            Some(Some(
                crate::toolchain::Host::current_exe()
                    .expect("the test binary path")
                    .into_os_string()
            )),
            "the wrapper must name this binary"
        );
        assert_eq!(
            env(crate::workflows::rustc_wrapper::WRAPPER_MODE_ENV),
            Some(Some(OsString::from("1")))
        );
        assert_eq!(
            env(crate::workflows::rustc_wrapper::BUILD_STD_TARGET_ENV),
            Some(Some(OsString::from("aarch64-linux-android")))
        );
        let expected_dylib_dir = target_dir
            .join("aarch64-linux-android")
            .join("debug")
            .join("deps");
        assert_eq!(
            env(crate::workflows::rustc_wrapper::BUILD_STD_DYLIB_DIR_ENV),
            Some(Some(expected_dylib_dir.into_os_string()))
        );
        assert_eq!(
            env(crate::workflows::rustc_wrapper::WRAPPER_CHAIN_ENV),
            Some(Some(OsString::from("/fake/sccache"))),
            "a configured sccache chains behind the shim"
        );
        // A workspace wrapper would replace RUSTC_WRAPPER on exactly the
        // link-emitting member units, so both spellings must be removed.
        assert_eq!(env("RUSTC_WORKSPACE_WRAPPER"), Some(None));
        assert_eq!(env("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"), Some(None));
    }

    #[test]
    fn apple_platform_dylibs_use_macho_extension() {
        assert_eq!(
            lib_extension_for_triple(&triple("aarch64-apple-darwin")),
            "dylib"
        );
        assert_eq!(
            lib_extension_for_triple(&triple("aarch64-apple-ios-sim")),
            "dylib"
        );
        assert_eq!(
            lib_extension_for_triple(&triple("aarch64-apple-ios")),
            "dylib"
        );
    }

    #[test]
    fn non_apple_platform_dylibs_keep_platform_extensions() {
        assert_eq!(
            lib_extension_for_triple(&triple("aarch64-linux-android")),
            "so"
        );
        assert_eq!(
            lib_extension_for_triple(&triple("x86_64-unknown-linux-gnu")),
            "so"
        );
        assert_eq!(
            lib_extension_for_triple(&triple("x86_64-pc-windows-msvc")),
            "dll"
        );
    }

    #[test]
    fn development_and_packaging_have_distinct_linkage() {
        assert_eq!(
            BuildOptions::development(BuildProfile::Debug).linkage(),
            RustLinkage::SharedRuntime
        );
        assert_eq!(
            BuildOptions::packaging(BuildProfile::Debug).linkage(),
            RustLinkage::Static
        );
        assert!(BuildOptions::development(BuildProfile::Release).is_release());
        assert!(BuildOptions::packaging(BuildProfile::Release).is_release());
    }

    #[test]
    fn build_profile_release_variants_select_the_release_profile() {
        assert!(BuildProfile::Release.is_release());
        assert!(BuildProfile::Profiling.is_release());
        assert!(!BuildProfile::Debug.is_release());
        assert!(!BuildProfile::Optimized.is_release());
    }

    #[test]
    fn development_profile_envs_realize_the_selected_trade_off() {
        let optimized = BuildOptions::development(BuildProfile::Optimized);
        let envs = optimized.cargo_envs();
        assert!(
            envs.contains(&(
                "CARGO_PROFILE_DEV_OPT_LEVEL".to_string(),
                OsString::from("1")
            )),
            "optimized development lifts the dev opt-level: {envs:?}"
        );
        assert!(
            envs.contains(&(
                "CARGO_PROFILE_DEV_DEBUG_ASSERTIONS".to_string(),
                OsString::from("false")
            )),
            "optimized development drops dep debug assertions: {envs:?}"
        );
        assert!(
            envs.contains(&(
                "CARGO_PROFILE_DEV_DEBUG".to_string(),
                OsString::from("true")
            )),
            "optimized development keeps full debug info: {envs:?}"
        );

        let profiling = BuildOptions::development(BuildProfile::Profiling);
        let envs = profiling.cargo_envs();
        for key in [
            "CARGO_PROFILE_RELEASE_OPT_LEVEL",
            "CARGO_PROFILE_RELEASE_DEBUG",
            "CARGO_PROFILE_RELEASE_STRIP",
        ] {
            assert!(
                envs.iter().any(|(env_key, _)| env_key == key),
                "profiling keeps debug info and symbols: missing {key} in {envs:?}"
            );
        }

        assert!(
            BuildOptions::development(BuildProfile::Debug)
                .cargo_envs()
                .is_empty(),
            "plain debug runs the declared dev profile"
        );
    }

    #[test]
    fn packaging_never_overrides_the_declared_profile() {
        for profile in [
            BuildProfile::Debug,
            BuildProfile::Optimized,
            BuildProfile::Release,
            BuildProfile::Profiling,
        ] {
            assert!(
                BuildOptions::packaging(profile).cargo_envs().is_empty(),
                "packaging {profile:?} must ship the declared profile"
            );
        }
    }

    #[test]
    fn resolves_target_standard_library_without_guessing_hash() {
        let directory = tempdir().expect("temporary target libdir");
        let android_triple = triple("aarch64-linux-android");
        let expected = directory.path().join("libstd-1234567890abcdef.so");
        std::fs::write(&expected, []).expect("write test std library");
        std::fs::write(directory.path().join("libcore.rlib"), []).expect("write unrelated library");

        assert_eq!(
            resolve_rust_standard_library_in(directory.path(), &android_triple)
                .expect("resolve dynamic std"),
            expected
        );
        assert_eq!(
            dynamic_library_file_name("waterui_dylib", &android_triple),
            "libwaterui_dylib.so"
        );
        assert_eq!(
            dynamic_library_file_name("waterui_dylib", &triple("x86_64-pc-windows-msvc")),
            "waterui_dylib.dll"
        );
    }

    #[test]
    fn compile_progress_classifies_cargo_unit_lines() {
        assert_eq!(
            classify_compile_line("   Compiling serde v1.0.228"),
            CompileEvent::Unit {
                phase: "Compiling",
                name: "serde".to_string(),
                version: Some("1.0.228".to_string()),
            }
        );
        assert_eq!(
            classify_compile_line("   Compiling waterui-app v0.1.0 (/tmp/app)"),
            CompileEvent::Unit {
                phase: "Compiling",
                name: "waterui-app".to_string(),
                version: Some("0.1.0".to_string()),
            }
        );
        assert_eq!(
            classify_compile_line("    Checking libc v0.2.171"),
            CompileEvent::Unit {
                phase: "Checking",
                name: "libc".to_string(),
                version: Some("0.2.171".to_string()),
            }
        );
    }

    #[test]
    fn compile_progress_keeps_non_unit_lines_verbatim() {
        assert_eq!(
            classify_compile_line("   Compiling 12 crates"),
            CompileEvent::Line("Compiling 12 crates".to_string())
        );
        assert_eq!(
            classify_compile_line("     Downloaded 300 crates (5.2 MB) in 1.23s"),
            CompileEvent::Line("Downloaded 300 crates (5.2 MB) in 1.23s".to_string())
        );
        assert_eq!(
            classify_compile_line(
                "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s"
            ),
            CompileEvent::Finished(
                "Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s".to_string()
            )
        );
        assert_eq!(
            classify_compile_line("warning: unused import"),
            CompileEvent::Line("warning: unused import".to_string())
        );
    }

    #[test]
    fn compile_progress_classifies_through_ansi_color() {
        // A user-forced `[term] color = "always"` or the CARGO_TERM_COLOR the
        // CLI sets for terminals wraps cargo's status words in escapes.
        let colored = "\u{1b}[0m\u{1b}[1m\u{1b}[32m   Compiling\u{1b}[0m serde v1.0.228";
        assert_eq!(
            classify_compile_line(colored),
            CompileEvent::Unit {
                phase: "Compiling",
                name: "serde".to_string(),
                version: Some("1.0.228".to_string()),
            }
        );
        let colored_finished =
            "\u{1b}[0m\u{1b}[1m\u{1b}[32m    Finished\u{1b}[0m `dev` profile in 1.23s";
        assert_eq!(
            classify_compile_line(colored_finished),
            CompileEvent::Finished(colored_finished.trim().to_string())
        );
    }

    #[test]
    fn static_packaging_removes_only_staged_android_runtime_libraries() {
        smol::block_on(async {
            let directory = tempdir().expect("temporary Android runtime directory");
            let android_triple = triple("aarch64-linux-android");
            for file_name in [
                "libwaterui_dylib.so",
                "libstd-old.so",
                "libwaterui_app.so",
                "libc++_shared.so",
            ] {
                std::fs::write(directory.path().join(file_name), [])
                    .expect("write staged runtime test file");
            }

            RustDynamicLibraries::remove_staged(directory.path(), &android_triple)
                .await
                .expect("remove shared Rust runtime libraries");

            assert!(!directory.path().join("libwaterui_dylib.so").exists());
            assert!(!directory.path().join("libstd-old.so").exists());
            assert!(directory.path().join("libwaterui_app.so").exists());
            assert!(directory.path().join("libc++_shared.so").exists());
        });
    }
}
