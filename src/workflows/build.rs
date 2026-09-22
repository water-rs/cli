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
use tracing::warn;

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

/// The rustup toolchain a project's builds run under: the one its own
/// directory selects, whatever directory the generated crate compiles in.
///
/// # Errors
/// Returns an error when rustup resolves no toolchain for the project.
pub async fn project_toolchain(project: &Project) -> eyre::Result<String> {
    Ok(crate::toolchain::rust::project_rustup_toolchain(project.root()).await?)
}

/// Resolve the Rust standard-library directory for a target triple under
/// `toolchain`, the rustup toolchain the libraries were built with.
///
/// # Errors
/// Returns an error if rustc cannot resolve an existing target library directory.
pub async fn rust_target_libdir(triple: &Triple, toolchain: &str) -> eyre::Result<PathBuf> {
    let target = triple.to_string();
    let host = crate::toolchain::Host::current().with_env("RUSTUP_TOOLCHAIN", toolchain);
    let output = host
        .run(
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
pub(crate) enum CargoTarget<'a> {
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

    /// Whether a `compiler-artifact` message's target is the one this build
    /// selected.
    fn matches(&self, target: &cargo_metadata::Target) -> bool {
        use cargo_metadata::TargetKind;
        match self {
            Self::Binary(name) => {
                target.name.as_str() == *name && target.kind.contains(&TargetKind::Bin)
            }
            Self::Lib => target.kind.iter().any(|kind| {
                matches!(
                    kind,
                    TargetKind::Lib
                        | TargetKind::RLib
                        | TargetKind::DyLib
                        | TargetKind::CDyLib
                        | TargetKind::StaticLib
                        | TargetKind::ProcMacro
                )
            }),
        }
    }
}

/// The outcome of one Cargo invocation: the profile directory everything
/// landed under and the artifact Cargo reported for the selected target.
#[derive(Debug)]
pub struct BuiltTarget {
    /// `<target>/<triple>/<profile>` — dependency artifacts and staged
    /// runtime libraries resolve from this directory.
    pub profile_dir: PathBuf,
    /// The final artifact Cargo reported writing for the selected target —
    /// its own `compiler-artifact` message, not a name reconstructed under
    /// the profile root.
    pub artifact: PathBuf,
    /// The `waterui-dylib` dynamic library Cargo reported, when this build
    /// produced one.
    pub shared_runtime: Option<PathBuf>,
}

impl BuiltTarget {
    /// Return the shared `WaterUI` runtime Cargo reported for this build.
    ///
    /// # Errors
    /// Returns an error when this build did not produce a shared runtime.
    pub fn shared_runtime(&self) -> eyre::Result<&Path> {
        self.shared_runtime.as_deref().ok_or_else(|| {
            eyre::eyre!(
                "Cargo reported no `waterui-dylib` dynamic library for the build in {}; the shared WaterUI runtime was not built",
                self.profile_dir.display()
            )
        })
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

/// Prepend the managed tool directories (`~/.water/tools/<name>/<version>`)
/// to the build's `PATH` so build scripts resolve a pinned `dxc`, JDK, and
/// friends by name — the user never edits `PATH`. A no-op when nothing is
/// installed (or the paths cannot join), so ambient `PATH` passes through.
fn with_managed_tools_path(command: &mut Command) {
    if let Some((key, value)) =
        crate::toolchain::managed_tool::managed_tools_path_env(&crate::toolchain::Host::current())
    {
        command.env(key, value);
    }
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
    /// The prebuilt `libstd` comes from the toolchain `project` selects — the
    /// one [`RustBuild`] compiled the runtime under — so the runtime and the
    /// shipped standard library agree; a runtime built under one toolchain
    /// and shipped with another's `libstd` fails at launch on the missing
    /// library hash.
    ///
    /// # Errors
    /// Returns an error when either required dynamic library is absent or ambiguous.
    pub async fn resolve(
        built: &BuiltTarget,
        triple: &Triple,
        project: &Project,
    ) -> eyre::Result<Self> {
        let waterui = built.shared_runtime()?.to_path_buf();
        let lib_dir = &built.profile_dir;

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
                let toolchain = project_toolchain(project).await?;
                let target_libdir = rust_target_libdir(triple, &toolchain).await?;
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
            let is_shader_compiler = triple.operating_system == OperatingSystem::Windows
                && matches!(file_name.as_ref(), "dxcompiler.dll" | "dxil.dll");
            if file_name == waterui
                || is_shader_compiler
                || (file_name.starts_with(standard_library_prefix)
                    && entry.path().extension().and_then(|value| value.to_str()) == Some(extension))
            {
                smol::fs::remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }
}

/// The shader-compiler runtime libraries a Windows binary `LoadLibrary`s by
/// name, resolved beside the `dxc` tool that ships them.
const DXC_RUNTIME_LIBRARIES: [&str; 2] = ["dxcompiler.dll", "dxil.dll"];

/// Resolve the `dxc` runtime pair to the copies installed beside the `dxc`
/// executable (on `PATH` or under the managed tool directory).
async fn resolve_dxc_runtime() -> eyre::Result<Vec<PathBuf>> {
    let host = crate::toolchain::Host::current();
    let dxc = crate::toolchain::dxc::Dxc
        .path(&host)
        .await
        .ok_or_else(|| {
            eyre::eyre!(
                "the dxc tool is not installed; run `water doctor` to install it, then build again"
            )
        })?;
    let dxc_dir = dxc.parent().ok_or_else(|| {
        eyre::eyre!(
            "the resolved dxc path {} has no parent directory",
            dxc.display()
        )
    })?;
    resolve_dxc_runtime_in(dxc_dir)
        .map_err(|error| eyre::eyre!("{}; run `water doctor` to reinstall dxc", error))
}

/// Collect [`DXC_RUNTIME_LIBRARIES`] from `dxc_dir`; a missing directory or
/// library is `NotFound`.
fn resolve_dxc_runtime_in(dxc_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    DXC_RUNTIME_LIBRARIES
        .iter()
        .map(|name| {
            let path = dxc_dir.join(name);
            path.is_file().then_some(path).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "the dxc installation at {} is missing {name}",
                        dxc_dir.display()
                    ),
                )
            })
        })
        .collect()
}

/// Stage the DirectX Shader Compiler runtime into `destination` for a Windows
/// binary whose wgpu DirectX 12 backend `LoadLibrary`s `dxcompiler.dll` and
/// `dxil.dll` by name at run time.
///
/// Windows resolves those names against the executable's directory before
/// `PATH`, so a staged copy always wins over a same-named library elsewhere
/// on `PATH` — and makes the binary self-contained on a machine without the
/// `dxc` tool installed.
///
/// # Errors
/// Returns an error when `dxc` is not installed, a runtime library is missing
/// beside it, or the destination cannot be created or written.
pub async fn stage_dxc_runtime(destination: &Path) -> eyre::Result<()> {
    smol::fs::create_dir_all(destination).await?;
    for source in resolve_dxc_runtime().await? {
        let file_name = source.file_name().ok_or_else(|| {
            eyre::eyre!("dxc runtime path has no file name: {}", source.display())
        })?;
        crate::utils::copy_file(&source, &destination.join(file_name))
            .await
            .wrap_err_with(|| {
                format!(
                    "Failed to stage {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
    }
    Ok(())
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
    /// info — the development default for self-drawn backends, whose
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
    ///
    /// A development build links the shared Rust runtime, a `dylib` crate,
    /// and a `dylib` links the toolchain's prebuilt `std`, which carries the
    /// `panic_unwind` runtime: under the packaging profile's `panic = "abort"`
    /// rustc refuses the link ("the linked panic runtime `panic_unwind` is
    /// not compiled with this crate's panic strategy `abort`"), and under its
    /// `lto = true` it refuses to prefer dynamic linking at all. The release
    /// profiles therefore unwind without LTO here; the packaging build keeps
    /// the manifest's `abort` and LTO.
    fn development_envs(self) -> Vec<(String, OsString)> {
        let entries: &[(&str, &str)] = match self {
            Self::Debug => &[],
            Self::Optimized => &[
                ("CARGO_PROFILE_DEV_OPT_LEVEL", "1"),
                ("CARGO_PROFILE_DEV_DEBUG", "true"),
                ("CARGO_PROFILE_DEV_DEBUG_ASSERTIONS", "false"),
                ("CARGO_PROFILE_DEV_OVERFLOW_CHECKS", "false"),
            ],
            Self::Release => &[
                ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "3"),
                ("CARGO_PROFILE_RELEASE_PANIC", "unwind"),
                ("CARGO_PROFILE_RELEASE_LTO", "off"),
            ],
            Self::Profiling => &[
                ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "3"),
                ("CARGO_PROFILE_RELEASE_PANIC", "unwind"),
                ("CARGO_PROFILE_RELEASE_LTO", "off"),
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
    pub fn with_static_runtime(mut self) -> Self {
        self.linkage = RustLinkage::Static;
        // No shared runtime to link, so the manifest's panic strategy and LTO
        // stand.
        self.cargo_envs.retain(|(key, _)| {
            key != "CARGO_PROFILE_RELEASE_PANIC" && key != "CARGO_PROFILE_RELEASE_LTO"
        });
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

    /// Build on behalf of `project`: its framework prepares the crate, and
    /// cargo runs under the rustup toolchain the project's own directory
    /// selects — the generated crate sits in the build cache, outside the
    /// project tree, where rustup would fall back to its default toolchain and
    /// link the runtime against a `libstd` the project's toolchain does not
    /// have.
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
    /// when the platform's loader needs them — embeds loader search paths into the
    /// final artifact only. A static packaging build needs none of this.
    ///
    /// A platform generally needs more than one: the binary is run both where it was
    /// built, with the runtime staged beside it, and from inside a packaged artifact
    /// that puts the runtime somewhere else. Each path becomes its own `-rpath`, and
    /// the loader tries them in order.
    #[must_use]
    pub fn with_linkage(
        self,
        linkage: RustLinkage,
        development_feature: &str,
        loader_search_paths: &[&str],
    ) -> Self {
        if linkage == RustLinkage::Static {
            return self;
        }
        let build = self
            .with_feature(development_feature)
            .with_preferred_dynamic_linking();
        loader_search_paths.iter().fold(build, |build, path| {
            build.with_final_rustc_arg(format!("-Clink-arg=-Wl,-rpath,{path}"))
        })
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
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If there was an error building the Rust library.
    pub async fn dev_build(&self) -> Result<BuiltTarget, RustBuildError> {
        self.build_lib(false).await
    }

    /// Build rust library in release mode.
    ///
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If there was an error building the Rust library.
    pub async fn release_build(&self) -> Result<BuiltTarget, RustBuildError> {
        self.build_lib(true).await
    }

    /// Build the crate's library target.
    ///
    /// The returned [`BuiltTarget`] carries the profile directory plus the
    /// artifact Cargo reported. A crate emitting several library crate types
    /// needs [`Self::with_crate_type_override`] to say which one is wanted —
    /// the build fails rather than guess.
    ///
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If there was an error building the Rust library.
    pub async fn build_lib(&self, release: bool) -> Result<BuiltTarget, RustBuildError> {
        self.build_inner(release, CargoTarget::Lib, self.lib_artifact_extension())
            .await
    }

    /// Build a dynamic library (cdylib) and return Cargo's reported build result.
    ///
    /// The path is Cargo's own `compiler-artifact` report, so the returned file
    /// is the one this build wrote even when another project's identically
    /// named crate shares the target directory.
    ///
    /// # Errors
    /// - `RustBuildError::FailToExecuteCargoBuild`: If there was an error executing the cargo build command.
    /// - `RustBuildError::FailToBuildRustLibrary`: If the library was not found after building.
    pub async fn build_dylib(&self, release: bool) -> Result<BuiltTarget, RustBuildError> {
        self.build_inner(
            release,
            CargoTarget::Lib,
            Some(lib_extension_for_triple(&self.triple)),
        )
        .await
    }

    /// Builds one named binary and returns its full output path.
    ///
    /// The path is Cargo's own `compiler-artifact` report (`executable` of the
    /// `--bin` unit), so it is the binary this build wrote even when another
    /// project's identically named crate shares the target directory.
    ///
    /// # Errors
    ///
    /// Returns an error when Cargo fails or the expected binary is missing.
    pub async fn build_binary(
        &self,
        binary_name: &str,
        release: bool,
    ) -> Result<BuiltTarget, RustBuildError> {
        self.build_inner(release, CargoTarget::Binary(binary_name), None)
            .await
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
        artifact_extension: Option<&'static str>,
    ) -> Result<BuiltTarget, RustBuildError> {
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

        // A dependency's final `dylib`/`cdylib` artifact uplifts to an
        // unhashed name (`deps/libwaterui_dylib.so`), so one filename serves
        // every same-named package sharing this target — last writer wins.
        // A `fresh` unit emits nothing yet still reports that path, which can
        // leave a different source's bytes where `water run` expects its own
        // runtime. The dep-info `.d` written alongside records the producing
        // sources; when they are not this unit's — or when no dep-info exists
        // to say — clean the package so the rebuild below emits this source's
        // artifact. The rebuild compiles the cleaned package anew, so a unit
        // it still reports `fresh` in the same state is a cache this CLI
        // cannot repair by rebuilding, and that is reported instead of retried.
        let stale = stale_shared_dylib_packages(&output.stdout).await?;
        if !stale.is_empty() {
            let target_dir = self.target_directory().await?;
            for unit in &stale {
                warn!(
                    package = unit.package,
                    artifact = %unit.artifact.display(),
                    "discarding a shared dylib unit and rebuilding it: {}",
                    unit.reason
                );
                clean_cargo_package(&self.path, &unit.package, &target_dir).await?;
            }
            output = self.cargo_build_output(release, cargo_target).await?;
            if !output.status.success() {
                let combined = combined_build_output(&output);
                return Err(RustBuildError::FailToBuildRustLibrary(
                    std::io::Error::other(format!(
                        "Cargo build failed:\n{}",
                        self.failure_report(&combined)
                    )),
                ));
            }
            let unrecovered = stale_shared_dylib_packages(&output.stdout).await?;
            if !unrecovered.is_empty() {
                return Err(unrecoverable_shared_dylib_error(&unrecovered, &target_dir));
            }
        }

        let artifact =
            reported_artifact(&output.stdout, &self.path, cargo_target, artifact_extension)?;
        let shared_runtime = reported_shared_runtime(&output.stdout)?;
        let profile_dir = self.lib_output_dir(release).await?;
        Ok(BuiltTarget {
            profile_dir,
            artifact,
            shared_runtime,
        })
    }

    /// The artifact extension this build's `--crate-type` override produces,
    /// when one is set and the type has a known file shape.
    fn lib_artifact_extension(&self) -> Option<&'static str> {
        self.crate_type_override
            .as_deref()
            .and_then(|crate_type| crate_type_artifact_extension(crate_type, &self.triple))
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
            .arg("--message-format=json-render-diagnostics")
            .args(cargo_target.cargo_args())
            .args(["--target", self.triple.to_string().as_str()])
            .args(framework.is_some().then_some("--locked"))
            .current_dir(&self.path);

        if let Some(target_dir) = &self.target_dir {
            cmd = cmd.arg("--target-dir").arg(target_dir);
        }
        with_managed_tools_path(cmd);
        // Apply extra environment variables (caller-provided values override defaults).
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        let mut cmd = self.with_project_toolchain_env(cmd).await?;

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
            crate::toolchain::sccache::configure_compilation_cache(cmd, sccache_path)
                .await
                .map_err(|error| {
                    RustBuildError::FailToBuildRustLibrary(std::io::Error::other(error.to_string()))
                })?;
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

    /// Run cargo under the rustup toolchain the project's own directory
    /// selects, so a crate generated outside the project tree (the build
    /// cache) compiles with the same toolchain as the project instead of
    /// rustup's default for that directory. A `-Zbuild-std` build names its
    /// own nightly through [`Self::with_build_std_envs`] instead.
    async fn with_project_toolchain_env<'a>(
        &self,
        cmd: &'a mut Command,
    ) -> Result<&'a mut Command, RustBuildError> {
        if self.build_std_toolchain.is_some() {
            return Ok(cmd);
        }
        let Some(project) = &self.project else {
            return Ok(cmd);
        };
        let toolchain = project_toolchain(project).await.map_err(|error| {
            RustBuildError::FailToBuildRustLibrary(std::io::Error::other(error.to_string()))
        })?;
        Ok(cmd.env("RUSTUP_TOOLCHAIN", toolchain))
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

/// The file extension the produced artifact carries for a `--crate-type`
/// value — `None` for a type with no single known file shape.
fn crate_type_artifact_extension(crate_type: &str, triple: &Triple) -> Option<&'static str> {
    match crate_type {
        "lib" | "rlib" => Some("rlib"),
        "staticlib" => Some(if matches!(triple.environment, Environment::Msvc) {
            "lib"
        } else {
            "a"
        }),
        "cdylib" | "dylib" | "proc-macro" => Some(lib_extension_for_triple(triple)),
        _ => None,
    }
}

/// The final artifact Cargo reported for the selected target: the
/// `compiler-artifact` message for `crate_dir`'s manifest, matched by target
/// kind — Cargo's own report of what it wrote, never a name reconstructed
/// under the profile directory.
///
/// Every generated crate builds into one shared per-user Cargo target, so
/// `<profile>/<name>` alone is not evidence the file came from this build.
/// `artifact_extension` disambiguates a library target that emitted several
/// crate types; without one, the build reports exactly one file or this
/// fails rather than guesses.
///
/// # Errors
/// Returns an error when no `compiler-artifact` message for the selected
/// target reports a matching file, or the reported file does not exist.
pub(crate) fn reported_artifact(
    stdout: &[u8],
    crate_dir: &Path,
    cargo_target: CargoTarget<'_>,
    artifact_extension: Option<&'static str>,
) -> Result<PathBuf, RustBuildError> {
    let manifest_path = dunce::canonicalize(crate_dir.join("Cargo.toml")).map_err(|error| {
        RustBuildError::FailToBuildRustLibrary(io::Error::other(format!(
            "failed to canonicalize {}: {error}",
            crate_dir.join("Cargo.toml").display()
        )))
    })?;
    let mut artifacts = Vec::new();
    for artifact in compiler_artifacts(stdout)? {
        if cargo_target.matches(&artifact.target)
            && same_manifest_path(artifact.manifest_path.as_std_path(), &manifest_path)
        {
            artifacts.push(artifact);
        }
    }
    reported_artifact_file(&artifacts, cargo_target, artifact_extension, &manifest_path)
}

/// Every `compiler-artifact` message in a cargo `--message-format=json`
/// stdout stream.
///
/// Cargo's report is the only record of what a build wrote, so a line naming
/// itself `compiler-artifact` that does not deserialize is a hard error
/// carrying the line — silently dropping it degrades into a misleading "no
/// artifact reported" failure downstream. Messages with any other `reason`,
/// and lines that are not cargo messages at all, are ignored.
pub(crate) fn compiler_artifacts(
    stdout: &[u8],
) -> Result<Vec<cargo_metadata::Artifact>, RustBuildError> {
    /// The one field that classifies a cargo message line.
    #[derive(serde::Deserialize)]
    struct Reason {
        reason: String,
    }

    let mut artifacts = Vec::new();
    for (index, line) in stdout.split(|byte| *byte == b'\n').enumerate() {
        let Ok(line) = str::from_utf8(line) else {
            continue;
        };
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let malformed = |error: serde_json::Error| {
            RustBuildError::FailToBuildRustLibrary(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cargo emitted a malformed `compiler-artifact` message on line {}: {error}\n{line}",
                    index + 1
                ),
            ))
        };
        match serde_json::from_str::<Reason>(line) {
            Ok(Reason { reason }) if reason == "compiler-artifact" => {
                let artifact =
                    serde_json::from_str::<cargo_metadata::Artifact>(line).map_err(malformed)?;
                artifacts.push(artifact);
            }
            // A line that is not readable JSON cannot yield its `reason`
            // field; one that still names itself a `compiler-artifact`
            // carries an unreadable payload — the hard error, never a drop.
            Err(error) if line.contains("\"reason\":\"compiler-artifact\"") => {
                return Err(malformed(error));
            }
            Ok(_) | Err(_) => {}
        }
    }
    Ok(artifacts)
}

fn reported_shared_runtime(stdout: &[u8]) -> Result<Option<PathBuf>, RustBuildError> {
    let mut reported = Vec::new();
    for artifact in compiler_artifacts(stdout)? {
        if artifact_package_name(&artifact.package_id) != "waterui-dylib"
            || !artifact
                .target
                .kind
                .contains(&cargo_metadata::TargetKind::DyLib)
        {
            continue;
        }
        for filename in &artifact.filenames {
            let path = filename.as_std_path();
            if is_dynamic_library(path) {
                reported.push((path.to_path_buf(), artifact.manifest_path.clone()));
            }
        }
    }
    match reported.as_slice() {
        [] => Ok(None),
        [(path, _)] => Ok(Some(path.clone())),
        _ => Err(RustBuildError::FailToBuildRustLibrary(io::Error::other(
            format!(
                "Cargo reported multiple `waterui-dylib` dynamic libraries: {}",
                reported
                    .iter()
                    .map(|(_, manifest)| manifest.as_std_path().display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))),
    }
}

/// Whether a `manifest_path` cargo reported is `expected`, the manifest of
/// the crate this build ran. Cargo reports the path in the spelling its own
/// working directory carried — a verbatim `\\?\` or an 8.3 short-name root on
/// Windows — so a lexical miss canonicalizes the reported path (it exists;
/// cargo just built from it) before deciding.
pub(crate) fn same_manifest_path(reported: &Path, expected: &Path) -> bool {
    reported == expected
        || dunce::canonicalize(reported).is_ok_and(|canonical| canonical == expected)
}

/// Picks the single file the selected target emitted out of its collected
/// `compiler-artifact` messages.
fn reported_artifact_file(
    artifacts: &[cargo_metadata::Artifact],
    cargo_target: CargoTarget<'_>,
    artifact_extension: Option<&'static str>,
    manifest_path: &Path,
) -> Result<PathBuf, RustBuildError> {
    let what = || -> String {
        match cargo_target {
            CargoTarget::Lib => format!("the library target of {}", manifest_path.display()),
            CargoTarget::Binary(name) => {
                format!("binary `{name}` of {}", manifest_path.display())
            }
        }
    };
    let not_found = |detail: String| {
        RustBuildError::FailToBuildRustLibrary(io::Error::new(io::ErrorKind::NotFound, detail))
    };

    let files: Vec<PathBuf> = artifacts
        .iter()
        .flat_map(|artifact| {
            artifact
                .filenames
                .iter()
                .map(|file| file.as_std_path().to_path_buf())
        })
        .collect();
    let artifact = match cargo_target {
        CargoTarget::Binary(_) => artifacts
            .iter()
            .find_map(|artifact| artifact.executable.as_ref())
            .map(|path| path.as_std_path().to_path_buf())
            .ok_or_else(|| {
                not_found(format!(
                    "Cargo reported no artifact for {} (reported files: {files:?})",
                    what()
                ))
            })?,
        CargoTarget::Lib => {
            let matching: Vec<&PathBuf> = artifact_extension.map_or_else(
                || files.iter().collect(),
                |extension| {
                    files
                        .iter()
                        .filter(|file| file.extension().is_some_and(|e| *e == *extension))
                        .collect()
                },
            );
            match matching.as_slice() {
                [only] => (*only).clone(),
                _ => {
                    return Err(not_found(artifact_extension.map_or_else(
                        || {
                            format!(
                                "Cargo reported {} artifacts for {} — select one with a crate-type override (reported files: {files:?})",
                                matching.len(),
                                what()
                            )
                        },
                        |extension| {
                            format!(
                                "Cargo reported no `.{extension}` artifact for {} (reported files: {files:?})",
                                what()
                            )
                        },
                    )));
                }
            }
        }
    };
    if !artifact.is_file() {
        return Err(not_found(format!(
            "Cargo reported {} for {} but the file does not exist",
            artifact.display(),
            what()
        )));
    }
    Ok(artifact)
}

/// Why a `fresh` shared dylib unit cannot be trusted as this project's own.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StaleSharedDylibReason {
    /// The dep-info beside the artifact names no source under the unit's
    /// manifest root: another source's build of the same-named package wrote
    /// the file.
    ForeignDepInfo { dep_info: PathBuf },
    /// No dep-info exists beside the artifact or in its unit directory, so
    /// nothing records which sources produced the bytes on disk.
    MissingDepInfo { reported_files: Vec<PathBuf> },
}

impl std::fmt::Display for StaleSharedDylibReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForeignDepInfo { dep_info } => write!(
                f,
                "its dep-info {} names no source under this unit's manifest root, so another source's build wrote it",
                dep_info.display()
            ),
            Self::MissingDepInfo { reported_files } => write!(
                f,
                "no dep-info was found beside it or in its unit directory, so nothing records which sources produced it (reported files: {reported_files:?})"
            ),
        }
    }
}

/// A `fresh` shared dylib unit whose artifact this build must not trust, and
/// the package whose units are cleaned so the rebuild emits its own.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleSharedDylib {
    package: String,
    artifact: PathBuf,
    reason: StaleSharedDylibReason,
}

/// The error a build reports when a cleaned and rebuilt package still comes
/// back `fresh` in a state this CLI cannot trust: rebuilding once did not
/// repair the cache, so it names the target directory to drop rather than
/// rebuilding again.
fn unrecoverable_shared_dylib_error(
    stale: &[StaleSharedDylib],
    target_dir: &Path,
) -> RustBuildError {
    let units = stale.iter().fold(String::new(), |mut units, unit| {
        let _ = std::fmt::Write::write_fmt(
            &mut units,
            format_args!(
                "\n  - {} ({}): {}",
                unit.artifact.display(),
                unit.package,
                unit.reason
            ),
        );
        units
    });
    let message = format!(
        "Cargo still reports a shared dylib unit as fresh after its package was cleaned and rebuilt:{units}\nThe shared Cargo target directory {} cannot be repaired by rebuilding; remove it with `water gc build-cache --shared-target` and build again.",
        target_dir.display()
    );
    RustBuildError::FailToBuildRustLibrary(io::Error::other(message))
}

/// Dependency packages whose `fresh` dynamic-library unit reports an artifact
/// this build did not verifiably write, with the reason for each.
///
/// Dep-info is the one record that names the producing sources: the `.d`
/// Cargo writes beside an uplifted dylib lists the writer's inputs, while the
/// unit's own `manifest_path` says which source *this* graph resolved. A
/// dep-info that names no file under the unit's manifest root was produced by
/// a different source's build, and the unhashed artifact it accompanies does
/// not belong to this project. An uplifted dylib with no dep-info at all is
/// a cache state this CLI's own builds leave behind (observed on Windows),
/// and it is flagged the same way so the caller rebuilds the package instead
/// of trusting bytes nothing accounts for.
async fn stale_shared_dylib_packages(
    stdout: &[u8],
) -> Result<Vec<StaleSharedDylib>, RustBuildError> {
    let mut stale: Vec<StaleSharedDylib> = Vec::new();
    for artifact in compiler_artifacts(stdout)? {
        if !artifact.fresh {
            continue;
        }
        let Some(manifest_dir) = artifact.manifest_path.as_std_path().parent() else {
            continue;
        };
        // Only a `dylib`/`cdylib` unit uplifts to an unhashed, shareable
        // filename. A proc-macro's dylib keeps its metadata hash — the hash
        // covers the package id, so two sources never meet — and cargo's
        // build-dir layout stores it where no dep-info convention below
        // applies.
        if !uplifts_dynamic_library(&artifact.target) {
            continue;
        }
        let manifest_root = dunce::simplified(manifest_dir);
        let package = artifact_package_name(&artifact.package_id);
        let mut package_stale = None;
        for filename in &artifact.filenames {
            let file = filename.as_std_path();
            if !is_dynamic_library(file) {
                continue;
            }
            let Some(dep_info) = dep_info_path(file, &artifact.filenames) else {
                package_stale = Some(StaleSharedDylib {
                    package: package.to_owned(),
                    artifact: file.to_path_buf(),
                    reason: StaleSharedDylibReason::MissingDepInfo {
                        reported_files: artifact
                            .filenames
                            .iter()
                            .map(|reported| reported.as_std_path().to_path_buf())
                            .collect(),
                    },
                });
                break;
            };
            let contents = smol::fs::read_to_string(&dep_info).await.map_err(|error| {
                RustBuildError::FailToBuildRustLibrary(io::Error::other(format!(
                    "Cargo reported {} fresh but its dep-info {} is unreadable: {error}",
                    file.display(),
                    dep_info.display()
                )))
            })?;
            // A dep-info that names no prerequisite under this unit's own
            // manifest root was written by a different source's build; a rare
            // miss costs one package rebuild — never a wrong artifact.
            if !dep_info_prerequisites(&contents).iter().any(|source| {
                let source = if source.is_absolute() {
                    source.clone()
                } else {
                    manifest_dir.join(source)
                };
                dunce::simplified(&source).starts_with(manifest_root)
            }) {
                package_stale = Some(StaleSharedDylib {
                    package: package.to_owned(),
                    artifact: file.to_path_buf(),
                    reason: StaleSharedDylibReason::ForeignDepInfo { dep_info },
                });
                break;
            }
        }
        if let Some(unit) = package_stale
            && !stale.iter().any(|known| known.package == unit.package)
        {
            stale.push(unit);
        }
    }
    stale.sort_unstable_by(|left, right| left.package.cmp(&right.package));
    Ok(stale)
}

/// Whether `file` names a dynamically linked library — the artifact shape a
/// dependency's final target uplifts to one unhashed filename per name.
fn is_dynamic_library(file: &Path) -> bool {
    file.extension()
        .is_some_and(|extension| matches!(extension.to_str(), Some("so" | "dylib" | "dll")))
}

/// Whether the unit's final artifact is a dynamic library cargo uplifts to
/// an unhashed filename: a `dylib` or `cdylib` crate type. Proc-macro
/// crates are dynamic libraries too, but stay hashed and are never shared.
fn uplifts_dynamic_library(target: &cargo_metadata::Target) -> bool {
    target.crate_types.iter().any(|kind| {
        matches!(
            kind,
            cargo_metadata::CrateType::DyLib | cargo_metadata::CrateType::CDyLib
        )
    })
}

/// The dep-info `.d` cargo wrote for the unit that produced `artifact_file`,
/// found where each cargo layout puts it.
///
/// Measured on a `dylib` dependency and a `cdylib` root unit (cargo 1.98
/// stable and the 1.100 nightly build-dir layout, `--message-format=json`):
///
/// - stable writes `<profile>/deps/<name>.d` for both, beside the hashed
///   copy, and uplifts the root unit's as `<profile>/lib<name>.d`;
/// - the build-dir layout writes `<name>.d` in the unit's own
///   `build/<package>/<hash>/out/` directory — a directory the message names
///   only through the unit's other outputs (the `.rmeta`/`.rlib` a dependency
///   emits) — and still uplifts the root unit's as `<profile>/lib<name>.d`.
///
/// `sibling_files` are the unit's reported filenames; the first candidate
/// that exists wins, and no candidate means the caller reports the miss.
fn dep_info_path(
    artifact_file: &Path,
    sibling_files: &[cargo_metadata::camino::Utf8PathBuf],
) -> Option<PathBuf> {
    let file_stem = artifact_file.file_stem()?.to_str()?;
    let name = file_stem.strip_prefix("lib").unwrap_or(file_stem);
    let dir = artifact_file.parent()?;
    // Most specific first: the uplifted `lib<name>.d`, the stable `deps/`
    // copy, the unit directory a sibling output names, and only then a bare
    // `<name>.d` beside the artifact (which the hashed proc-macro layout
    // spells that way, and which a same-named bin would also write).
    let mut candidates = vec![
        dir.join(format!("{file_stem}.d")),
        dir.join("deps").join(format!("{name}.d")),
    ];
    candidates.extend(
        sibling_files
            .iter()
            .filter_map(|sibling| sibling.as_std_path().parent())
            .filter(|unit_dir| *unit_dir != dir)
            .map(|unit_dir| unit_dir.join(format!("{name}.d"))),
    );
    candidates.push(dir.join(format!("{name}.d")));
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// The prerequisite paths a dep-info `.d` lists.
///
/// Cargo writes Makefile syntax: one `<target>: <space-separated
/// prerequisites>` rule per emitted artifact, then an empty `<path>:` rule
/// per prerequisite. rustc's `escape_dep_filename`
/// (`compiler/rustc_interface/src/passes.rs`) escapes *only* a literal space
/// as `\ ` — every other byte, a Windows backslash or drive-letter colon
/// included, is verbatim — and Cargo's own `parse_rustc_dep_info`
/// (`src/cargo/core/compiler/fingerprint/dep_info.rs`) reads the same
/// contract: split a rule at its first `": "` — `C:\` is colon-then-
/// backslash and a literal `": "` inside a name arrives escaped `":\ "`, so
/// the separator is unambiguous — then treat a token's trailing `\` as the
/// escaped space joining it to the next token. rustc never emits `$$` or
/// `\\` escapes in prerequisites, so neither is unescaped here: doing so
/// would corrupt the verbatim bytes a Windows path carries. A `\` at the
/// end of a line is make's continuation and joins the next line before
/// tokenizing.
fn dep_info_prerequisites(contents: &str) -> Vec<PathBuf> {
    // Join `\<newline>` continuations into one logical line per rule before
    // anything looks for the `": "` separator.
    let mut joined = String::with_capacity(contents.len());
    for line in contents.lines() {
        if let Some(head) = line.strip_suffix('\\') {
            joined.push_str(head);
            joined.push(' ');
        } else {
            joined.push_str(line);
            joined.push('\n');
        }
    }
    let mut prerequisites = Vec::new();
    for line in joined.lines() {
        let Some((_, rest)) = line.split_once(": ") else {
            continue;
        };
        let mut token = String::new();
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    chars.next();
                    token.push(' ');
                }
                c if c.is_whitespace() => {
                    if !token.is_empty() {
                        prerequisites.push(PathBuf::from(std::mem::take(&mut token)));
                    }
                }
                c => token.push(c),
            }
        }
        if !token.is_empty() {
            prerequisites.push(PathBuf::from(token));
        }
    }
    prerequisites
}

/// The package name a `package_id` specifier carries — `source#name@version`,
/// or the source's final path segment for the older `source#version` form.
fn artifact_package_name(package_id: &cargo_metadata::PackageId) -> &str {
    let repr = package_id.repr.as_str();
    let (source, fragment) = repr.rsplit_once('#').unwrap_or((repr, ""));
    fragment.split_once('@').map_or_else(
        || source.rsplit('/').next().unwrap_or(repr),
        |(name, _)| name,
    )
}

/// `cargo clean -p <package>` in `crate_dir`, confined to `target_dir`: drops
/// the package's units — including the unhashed artifact another source's
/// build left behind — so the next build re-emits this graph's own.
async fn clean_cargo_package(
    crate_dir: &Path,
    package: &str,
    target_dir: &Path,
) -> Result<(), RustBuildError> {
    let mut command = Command::new("cargo");
    command
        .arg("clean")
        .arg("-p")
        .arg(package)
        .arg("--target-dir")
        .arg(target_dir)
        .current_dir(crate_dir);
    configure_generated_crate_compilation(&mut command);
    let output = command
        .output()
        .await
        .map_err(RustBuildError::FailToExecuteCargoBuild)?;
    if !output.status.success() {
        return Err(RustBuildError::FailToBuildRustLibrary(io::Error::other(
            format!(
                "cargo clean -p {package} failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ),
        )));
    }
    Ok(())
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
    use std::path::PathBuf;

    use super::{
        BuildOptions, BuildProfile, BuiltTarget, CargoTarget, CompileEvent, RustBuild,
        RustDynamicLibraries, RustLinkage, classify_compile_line, dynamic_library_file_name,
        lib_extension_for_triple, reported_shared_runtime, resolve_dxc_runtime_in,
        resolve_rust_standard_library_in,
    };

    fn shared_runtime_artifact_json(
        manifest: &std::path::Path,
        file: &std::path::Path,
        package: &str,
    ) -> String {
        serde_json::json!({
            "reason": "compiler-artifact",
            "package_id": format!("path+file:///x#{package}@0.1.0"),
            "manifest_path": manifest,
            "target": {
                "kind": ["dylib"],
                "crate_types": ["dylib"],
                "name": package,
                "src_path": manifest.parent().expect("manifest dir").join("src/lib.rs"),
                "edition": "2021",
                "doc": false,
                "doctest": false,
                "test": false,
            },
            "profile": {
                "opt_level": "0",
                "debuginfo": 0,
                "debug_assertions": true,
                "overflow_checks": true,
                "test": false,
            },
            "features": [],
            "filenames": [file],
            "executable": null,
            "fresh": true,
        })
        .to_string()
    }

    #[test]
    fn reported_shared_runtime_selects_waterui_dylib_dynamic_artifact() {
        let temporary = tempdir().expect("tempdir");
        let manifest = temporary.path().join("waterui-dylib/Cargo.toml");
        let runtime = temporary
            .path()
            .join("target/debug/deps/libwaterui_dylib.so");
        let unrelated_manifest = temporary.path().join("app/Cargo.toml");
        let unrelated = temporary.path().join("target/debug/app");
        let stdout = format!(
            "{}\n{}\n",
            shared_runtime_artifact_json(&unrelated_manifest, &unrelated, "app"),
            shared_runtime_artifact_json(&manifest, &runtime, "waterui-dylib"),
        );

        assert_eq!(
            reported_shared_runtime(stdout.as_bytes()).expect("runtime report"),
            Some(runtime)
        );
    }

    #[test]
    fn missing_shared_runtime_report_is_none_and_accessor_errors() {
        let temporary = tempdir().expect("tempdir");
        let stdout = shared_runtime_artifact_json(
            &temporary.path().join("app/Cargo.toml"),
            &temporary.path().join("target/debug/app"),
            "app",
        );
        assert_eq!(
            reported_shared_runtime(stdout.as_bytes()).expect("runtime report"),
            None
        );

        let profile_dir = temporary.path().join("target/debug");
        let error = BuiltTarget {
            profile_dir: profile_dir.clone(),
            artifact: temporary.path().join("app"),
            shared_runtime: None,
        }
        .shared_runtime()
        .expect_err("missing runtime should fail");
        let message = error.to_string();
        assert!(message.contains("waterui-dylib"));
        assert!(message.contains(&profile_dir.display().to_string()));
    }

    #[test]
    fn reported_shared_runtime_rejects_multiple_manifests() {
        let temporary = tempdir().expect("tempdir");
        let first_manifest = temporary.path().join("first/Cargo.toml");
        let second_manifest = temporary.path().join("second/Cargo.toml");
        let stdout = format!(
            "{}\n{}\n",
            shared_runtime_artifact_json(
                &first_manifest,
                &temporary.path().join("target/debug/libfirst.so"),
                "waterui-dylib",
            ),
            shared_runtime_artifact_json(
                &second_manifest,
                &temporary.path().join("target/debug/libsecond.so"),
                "waterui-dylib",
            ),
        );

        let error =
            reported_shared_runtime(stdout.as_bytes()).expect_err("ambiguous runtime report");
        let message = error.to_string();
        assert!(message.contains(&first_manifest.display().to_string()));
        assert!(message.contains(&second_manifest.display().to_string()));
    }

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

        let shared_runtime_envs = [
            (
                "CARGO_PROFILE_RELEASE_PANIC".to_string(),
                OsString::from("unwind"),
            ),
            (
                "CARGO_PROFILE_RELEASE_LTO".to_string(),
                OsString::from("off"),
            ),
        ];
        for env in &shared_runtime_envs {
            assert!(
                BuildOptions::development(BuildProfile::Release)
                    .cargo_envs()
                    .contains(env),
                "a release development build links the shared runtime: missing {env:?}"
            );
            assert!(
                !BuildOptions::development(BuildProfile::Release)
                    .with_static_runtime()
                    .cargo_envs()
                    .contains(env),
                "a static runtime keeps the manifest's {env:?}"
            );
        }
        let unwind = &shared_runtime_envs[0];

        let profiling = BuildOptions::development(BuildProfile::Profiling);
        let envs = profiling.cargo_envs();
        assert!(
            envs.contains(unwind),
            "profiling links the shared runtime too"
        );
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

    /// Two projects named `demo` in different directories generate crates
    /// whose package names differ by the project-root tag, so one shared
    /// Cargo target gives each its own uplifted artifact — and the build
    /// resolves it from Cargo's `compiler-artifact` report rather than a
    /// bare `<profile>/<name>` guess.
    #[test]
    fn same_named_projects_resolve_their_own_artifacts_in_one_shared_target() {
        use crate::project_model::project_types::{CrateName, generated_crate_name};

        smol::block_on(async {
            let temporary = tempdir().expect("tempdir");
            let shared_target = temporary.path().join("shared-target");
            let demo = CrateName::try_from("demo").expect("crate name");
            let mut artifacts = Vec::new();
            for (directory, marker) in [("first", "first"), ("second", "second")] {
                let project_root = temporary.path().join(directory);
                let crate_dir = project_root.join("hydrolysis");
                std::fs::create_dir_all(crate_dir.join("src")).expect("crate dir");
                let package = generated_crate_name(&demo, "hydrolysis", &project_root);
                std::fs::write(
                    crate_dir.join("Cargo.toml"),
                    format!(
                        "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
                    ),
                )
                .expect("manifest");
                std::fs::write(
                    crate_dir.join("src/main.rs"),
                    format!("fn main() {{ println!(\"{marker}\"); }}\n"),
                )
                .expect("main.rs");

                let artifact = super::RustBuild::new(&crate_dir, Triple::host())
                    .with_target_dir(&shared_target)
                    .build_binary(package.as_str(), false)
                    .await
                    .expect("the generated crate builds")
                    .artifact;
                assert!(artifact.is_file(), "the reported artifact exists");
                artifacts.push(artifact);
            }

            assert_ne!(
                artifacts[0], artifacts[1],
                "each same-named project resolves its own artifact"
            );
            for (artifact, marker) in artifacts.iter().zip(["first", "second"]) {
                let ran = std::process::Command::new(artifact)
                    .output()
                    .expect("the resolved artifact executes");
                assert_eq!(
                    String::from_utf8_lossy(&ran.stdout).trim(),
                    marker,
                    "the artifact is this project's binary, not the sibling's"
                );
            }
        });
    }

    /// `reported_artifact` matches on the artifact's manifest path — the
    /// identity Cargo assigns the unit — and returns the file the message
    /// reports even when that path is the hash-suffixed `deps/` copy, so a
    /// sibling package's artifact in the same stream is never picked up.
    #[test]
    fn reported_artifact_selects_the_matching_manifests_file() {
        let temporary = tempdir().expect("tempdir");
        let crate_dir = temporary.path().join("demo-hydrolysis-deadbeef");
        std::fs::create_dir_all(&crate_dir).expect("crate dir");
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]\n").expect("manifest");
        let manifest =
            dunce::canonicalize(crate_dir.join("Cargo.toml")).expect("canonical manifest");
        let reported = crate_dir.join("target/debug/deps/demo_hydrolysis_deadbeef-abc123.rlib");
        std::fs::create_dir_all(reported.parent().expect("deps dir")).expect("deps dir");
        std::fs::write(&reported, []).expect("reported artifact");

        // The messages are serialized, never formatted: a `Path` must land in
        // the JSON as an escaped string, which `display()` cannot do on
        // Windows where paths carry backslashes.
        let artifact_json = |manifest: &std::path::Path, file: &std::path::Path, name: &str| {
            serde_json::json!({
                "reason": "compiler-artifact",
                "package_id": format!("path+file:///x#{name}@0.1.0"),
                "manifest_path": manifest,
                "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": name,
                    "src_path": manifest.parent().expect("manifest dir").join("src/lib.rs"),
                    "edition": "2021",
                    "doc": true,
                    "doctest": true,
                    "test": true,
                },
                "profile": {
                    "opt_level": "0",
                    "debuginfo": 0,
                    "debug_assertions": true,
                    "overflow_checks": true,
                    "test": false,
                },
                "features": [],
                "filenames": [file],
                "executable": null,
                "fresh": true,
            })
            .to_string()
        };

        let other_manifest = temporary.path().join("other").join("Cargo.toml");
        let other_file = temporary.path().join("other.rlib");
        let stdout = format!(
            "{}\n{}\n",
            artifact_json(&other_manifest, &other_file, "other"),
            artifact_json(&manifest, &reported, "demo_hydrolysis_deadbeef"),
        );
        let resolved = super::reported_artifact(
            stdout.as_bytes(),
            &crate_dir,
            CargoTarget::Lib,
            Some("rlib"),
        )
        .expect("the matching manifest's artifact resolves");
        assert_eq!(resolved, reported);

        let foreign_only = artifact_json(&other_manifest, &other_file, "other");
        assert!(
            super::reported_artifact(
                foreign_only.as_bytes(),
                &crate_dir,
                CargoTarget::Lib,
                Some("rlib"),
            )
            .is_err(),
            "an artifact for another manifest is never selected"
        );
    }

    /// A dependency's uplifted dylib is unhashed, so a `fresh` report does not
    /// prove the file is this source's — the dep-info beside it records the
    /// producing sources, and only a dep-info naming this unit's own manifest
    /// root clears it.
    #[test]
    fn stale_shared_dylib_packages_flags_a_foreign_written_artifact() {
        smol::block_on(async {
            let temporary = tempdir().expect("tempdir");
            let deps = temporary.path().join("debug/deps");
            std::fs::create_dir_all(&deps).expect("deps dir");
            let dylib = deps.join("libwaterui_dylib.so");
            std::fs::write(&dylib, []).expect("dylib");

            // The manifest root carries a space so the dep-info fixture
            // exercises the `\ ` escape end to end: the written prerequisite
            // must still resolve to this root.
            let ours = temporary.path().join("our project");
            std::fs::create_dir_all(ours.join("src")).expect("our manifest dir");
            let manifest = ours.join("Cargo.toml");
            std::fs::write(&manifest, "").expect("manifest");
            let own_source = ours.join("src/lib.rs");
            std::fs::write(&own_source, "").expect("own source");

            let artifact = |fresh: bool| {
                serde_json::json!({
                    "reason": "compiler-artifact",
                    "package_id": "path+file:///x#waterui-dylib@0.1.0",
                    "manifest_path": manifest,
                    "target": {
                        "kind": ["lib"],
                        "crate_types": ["dylib"],
                        "name": "waterui_dylib",
                        "src_path": own_source,
                        "edition": "2021",
                        "doc": true,
                        "doctest": true,
                        "test": true,
                    },
                    "profile": {
                        "opt_level": "0",
                        "debuginfo": 0,
                        "debug_assertions": true,
                        "overflow_checks": true,
                        "test": false,
                    },
                    "features": [],
                    "filenames": [dylib],
                    "executable": null,
                    "fresh": fresh,
                })
                .to_string()
            };
            let dep_info = deps.join("waterui_dylib.d");

            // Dep-info rides in rustc's Makefile spelling: a literal space in
            // a path is `\ ` and every other byte is verbatim, so the fixture
            // writes real tempdir paths through the same escaping.
            let foreign = temporary.path().join("foreign");
            std::fs::create_dir_all(foreign.join("src")).expect("foreign source dir");
            let foreign_source = foreign.join("src/lib.rs");
            std::fs::write(&foreign_source, "").expect("foreign source");
            let dep_escape =
                |path: &std::path::Path| path.display().to_string().replace(' ', "\\ ");
            let write_dep_info = |source: &std::path::Path| {
                std::fs::write(
                    &dep_info,
                    format!("{}: {}\n", dep_escape(&dylib), dep_escape(source)),
                )
                .expect("dep-info");
            };

            // A `fresh` unit whose dep-info names another source's checkout.
            write_dep_info(&foreign_source);
            let stale = super::stale_shared_dylib_packages(artifact(true).as_bytes())
                .await
                .expect("scan");
            assert_eq!(
                stale,
                [super::StaleSharedDylib {
                    package: "waterui-dylib".to_owned(),
                    artifact: dylib.clone(),
                    reason: super::StaleSharedDylibReason::ForeignDepInfo {
                        dep_info: dep_info.clone(),
                    },
                }]
            );

            // The same file written by this unit's own source is trusted.
            write_dep_info(&own_source);
            let stale = super::stale_shared_dylib_packages(artifact(true).as_bytes())
                .await
                .expect("scan");
            assert!(stale.is_empty(), "our own artifact is never stale");

            // A unit cargo just emitted needs no dep-info check at all.
            write_dep_info(&foreign_source);
            let stale = super::stale_shared_dylib_packages(artifact(false).as_bytes())
                .await
                .expect("scan");
            assert!(stale.is_empty(), "a non-fresh unit wrote the file itself");
        });
    }

    /// Cargo's build-dir layout (nightly 1.100) writes a unit's dep-info in
    /// `build/<package>/<hash>/out/` beside its other outputs instead of
    /// `<profile>/deps/`; the unit's `.rmeta` names that directory. A fresh
    /// proc-macro unit — hashed, never uplifted, and on that layout without
    /// any dep-info the `deps/` convention could find — takes no part.
    #[test]
    fn stale_check_reads_build_dir_dep_info_and_skips_proc_macros() {
        smol::block_on(async {
            let temporary = tempdir().expect("tempdir");
            let profile = temporary.path().join("debug");
            let unit_dir = profile.join("build/waterui-dylib/0123456789abcdef/out");
            std::fs::create_dir_all(&unit_dir).expect("unit dir");
            let dylib = profile.join("libwaterui_dylib.so");
            std::fs::write(&dylib, []).expect("dylib");
            let rmeta = unit_dir.join("libwaterui_dylib.rmeta");
            std::fs::write(&rmeta, []).expect("rmeta");

            let ours = temporary.path().join("ours");
            std::fs::create_dir_all(ours.join("src")).expect("our manifest dir");
            let manifest = ours.join("Cargo.toml");
            std::fs::write(&manifest, "").expect("manifest");
            let foreign = temporary.path().join("foreign/src/lib.rs");
            std::fs::create_dir_all(foreign.parent().expect("parent")).expect("foreign dir");
            std::fs::write(&foreign, []).expect("foreign source");
            std::fs::write(
                unit_dir.join("waterui_dylib.d"),
                format!("{}: {}\n", dylib.display(), foreign.display()),
            )
            .expect("dep-info");

            let unit = |name: &str, crate_type: &str, filenames: Vec<&std::path::Path>| {
                serde_json::json!({
                    "reason": "compiler-artifact",
                    "package_id": format!("path+file:///x#{name}@0.1.0"),
                    "manifest_path": manifest,
                    "target": {
                        "kind": [if crate_type == "proc-macro" { "proc-macro" } else { "lib" }],
                        "crate_types": [crate_type],
                        "name": name.replace('-', "_"),
                        "src_path": ours.join("src/lib.rs"),
                        "edition": "2021",
                        "doc": true,
                        "doctest": true,
                        "test": true,
                    },
                    "profile": {
                        "opt_level": "0",
                        "debuginfo": 0,
                        "debug_assertions": true,
                        "overflow_checks": true,
                        "test": false,
                    },
                    "features": [],
                    "filenames": filenames,
                    "executable": null,
                    "fresh": true,
                })
                .to_string()
            };
            // The proc-macro's dylib exists nowhere on disk and has no
            // dep-info; only the dylib unit is examined, and its dep-info is
            // found through the `.rmeta` sibling's directory.
            let macro_dylib = unit_dir.join("libthiserror_impl-0123456789abcdef.so");
            let stdout = format!(
                "{}\n{}\n",
                unit("thiserror-impl", "proc-macro", vec![&macro_dylib]),
                unit("waterui-dylib", "dylib", vec![&dylib, &rmeta]),
            );
            let stale = super::stale_shared_dylib_packages(stdout.as_bytes())
                .await
                .expect("scan");
            assert_eq!(stale.len(), 1, "{stale:?}");
            assert_eq!(stale[0].package, "waterui-dylib");
            assert!(
                matches!(
                    stale[0].reason,
                    super::StaleSharedDylibReason::ForeignDepInfo { .. }
                ),
                "{:?}",
                stale[0].reason
            );
        });
    }

    /// A fresh uplifted dylib with no dep-info beside it or in its unit
    /// directory is a cache this CLI wrote and can no longer account for. It
    /// is recovered — flagged so the package is cleaned and rebuilt — rather
    /// than reported as an error, and the reason names the missing record.
    #[test]
    fn fresh_uplifted_dylib_without_dep_info_is_recovered_not_reported() {
        smol::block_on(async {
            let temporary = tempdir().expect("tempdir");
            let profile = temporary
                .path()
                .join("target/shared/x86_64-pc-windows-msvc/debug");
            let deps = profile.join("deps");
            std::fs::create_dir_all(&deps).expect("deps dir");
            let dylib = profile.join("waterui_dylib.dll");
            std::fs::write(&dylib, []).expect("dylib");
            let import_lib = profile.join("waterui_dylib.dll.lib");
            std::fs::write(&import_lib, []).expect("import lib");

            let ours = temporary.path().join("ours");
            std::fs::create_dir_all(ours.join("src")).expect("our manifest dir");
            let manifest = ours.join("Cargo.toml");
            std::fs::write(&manifest, "").expect("manifest");

            let stdout = serde_json::json!({
                "reason": "compiler-artifact",
                "package_id": "path+file:///x#waterui-dylib@0.1.0",
                "manifest_path": manifest,
                "target": {
                    "kind": ["lib"],
                    "crate_types": ["dylib"],
                    "name": "waterui_dylib",
                    "src_path": ours.join("src/lib.rs"),
                    "edition": "2021",
                    "doc": true,
                    "doctest": true,
                    "test": true,
                },
                "profile": {
                    "opt_level": "0",
                    "debuginfo": 0,
                    "debug_assertions": true,
                    "overflow_checks": true,
                    "test": false,
                },
                "features": [],
                "filenames": [dylib, import_lib],
                "executable": null,
                "fresh": true,
            })
            .to_string();

            let stale = super::stale_shared_dylib_packages(stdout.as_bytes())
                .await
                .expect("a fresh dylib without dep-info is recovered, not reported");
            assert_eq!(
                stale,
                [super::StaleSharedDylib {
                    package: "waterui-dylib".to_owned(),
                    artifact: dylib.clone(),
                    reason: super::StaleSharedDylibReason::MissingDepInfo {
                        reported_files: vec![dylib.clone(), import_lib.clone()],
                    },
                }]
            );
            let reason = stale[0].reason.to_string();
            assert!(reason.contains("no dep-info was found"), "{reason}");

            // A second pass in the same state after the rebuild is the loud
            // failure, naming the artifact, the package, and the target
            // directory to drop.
            let target_dir = temporary.path().join("target/shared");
            let error = super::unrecoverable_shared_dylib_error(&stale, &target_dir).to_string();
            assert!(
                error.contains("after its package was cleaned and rebuilt"),
                "{error}"
            );
            assert!(error.contains("waterui-dylib"), "{error}");
            assert!(error.contains(&dylib.display().to_string()), "{error}");
            assert!(error.contains(&target_dir.display().to_string()), "{error}");
        });
    }

    /// Dep-info prerequisites arrive in Makefile spelling: `\ ` escapes a
    /// literal space, a `\` at end of line continues the rule, and a Windows
    /// drive-letter colon is data — only the first `": "` separates the
    /// target. rustc escapes nothing else, so `$$` and `\\` stay verbatim.
    #[test]
    fn dep_info_prerequisites_unescape_spaces_and_join_continued_rules() {
        let contents = concat!(
            "C:\\out\\app.dll: C:\\work\\my\\ app\\src\\lib.rs \\\n",
            "    C:\\work\\my\\ app\\build.rs C:\\work\\cost$$.rs\n",
            "\n",
            "C:\\work\\my\\ app\\src\\lib.rs:\n",
        );
        assert_eq!(
            super::dep_info_prerequisites(contents),
            vec![
                PathBuf::from("C:\\work\\my app\\src\\lib.rs"),
                PathBuf::from("C:\\work\\my app\\build.rs"),
                PathBuf::from("C:\\work\\cost$$.rs"),
            ]
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

    #[test]
    fn dxc_runtime_resolution_collects_the_pair_beside_dxc() {
        let directory = tempdir().expect("temporary dxc directory");
        for name in ["dxcompiler.dll", "dxil.dll"] {
            std::fs::write(directory.path().join(name), []).expect("write runtime stub");
        }

        assert_eq!(
            resolve_dxc_runtime_in(directory.path()).expect("resolve dxc runtime pair"),
            vec![
                directory.path().join("dxcompiler.dll"),
                directory.path().join("dxil.dll"),
            ]
        );
    }

    #[test]
    fn dxc_runtime_resolution_names_the_missing_library() {
        let directory = tempdir().expect("temporary dxc directory");
        std::fs::write(directory.path().join("dxcompiler.dll"), []).expect("write runtime stub");

        let error = resolve_dxc_runtime_in(directory.path())
            .expect_err("a missing dxil.dll must fail resolution");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(error.to_string().contains("dxil.dll"), "{error}");
    }

    #[test]
    fn static_packaging_removes_staged_shader_compiler_libraries() {
        smol::block_on(async {
            let directory = tempdir().expect("temporary Windows runtime directory");
            let windows_triple = triple("x86_64-pc-windows-msvc");
            for file_name in [
                "waterui_dylib.dll",
                "std-1234567890abcdef.dll",
                "dxcompiler.dll",
                "dxil.dll",
                "keep.dll",
            ] {
                std::fs::write(directory.path().join(file_name), [])
                    .expect("write staged runtime test file");
            }

            RustDynamicLibraries::remove_staged(directory.path(), &windows_triple)
                .await
                .expect("remove shared Rust runtime libraries");

            for file_name in [
                "waterui_dylib.dll",
                "std-1234567890abcdef.dll",
                "dxcompiler.dll",
                "dxil.dll",
            ] {
                assert!(!directory.path().join(file_name).exists(), "{file_name}");
            }
            assert!(directory.path().join("keep.dll").exists());
        });
    }
}
