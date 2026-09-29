//! Apple platform build and package utilities.
//!
//! This module provides utility functions for building and packaging Apple apps.
//! These functions are used by `AppleBackend` to implement the `Backend` trait.

use std::path::{Path, PathBuf};

use eyre::{Context, bail};
use smol::fs;
use tracing::info;

#[cfg(target_os = "macos")]
use crate::browser_runtime;
#[cfg(target_os = "macos")]
use crate::macos_bundle::{package_cef_helper_app, remove_cef_helper_apps};
use crate::{
    apple::app_bundle,
    apple::backend::AppleBackend,
    apple::dynamic_runtime,
    assets,
    build::{BuildOptions, BuiltTarget, RustBuild, RustDynamicLibraries, RustLinkage},
    device::Artifact,
    platform::{PackageOptions, TargetBackend, TargetPlatform},
    project::{BrowserRuntimePlan, Project, ResolvedWebViewBackend},
    utils::copy_file,
};

/// The generated FFI crate's application binary — the `[[bin]]` target
/// `src/bin/waterui-apple-main.rs` declares.
///
/// Entry-owning Apple packaging installs it as the bundle executable; the
/// library target stays for the embedding path.
pub const APPLE_ENTRY_BINARY_NAME: &str = "waterui-apple-main";

// ============================================================================
// Build Utilities
// ============================================================================

/// The library shape an Apple build hands to Xcode.
///
/// A packaged app links the runtime into itself and needs a self-contained archive. A
/// development build resolves the runtime from `libwaterui_dylib.dylib` at load time, so
/// the archive's contents are redundant there: `ld` satisfies the symbols from the dylib
/// and pulls almost nothing out of the archive, which is why the shipped executable comes
/// out around 19 MB from a 428 MB input. Emitting a `cdylib` instead expresses the same
/// final link without materializing the archive at all — 9.8 MB instead of 428 MB, and
/// proportionally less I/O on machines whose storage is slower than the one this was
/// measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppleHostLibrary {
    /// Self-contained archive linked into a packaged application.
    Archive,
    /// Shared library that resolves the `WaterUI` runtime at load time.
    Dynamic,
}

impl AppleHostLibrary {
    const fn for_linkage(linkage: RustLinkage) -> Self {
        match linkage {
            RustLinkage::Static => Self::Archive,
            RustLinkage::SharedRuntime => Self::Dynamic,
        }
    }

    const fn crate_type(self) -> &'static str {
        match self {
            Self::Archive => "staticlib",
            Self::Dynamic => "cdylib",
        }
    }

    /// Name Xcode links against, via `-lwaterui_app` in `OTHER_LDFLAGS`.
    const fn linked_file_name(self) -> &'static str {
        match self {
            Self::Archive => "libwaterui_app.a",
            Self::Dynamic => "libwaterui_app.dylib",
        }
    }

    /// The shape this build must delete, so `-lwaterui_app` cannot resolve to a stale
    /// artifact left by a build of the other kind.
    const fn superseded(self) -> Self {
        match self {
            Self::Archive => Self::Dynamic,
            Self::Dynamic => Self::Archive,
        }
    }
}

/// Remove the host library shape this build did not produce.
///
/// `-lwaterui_app` resolves against whatever sits in the products directory, and `ld`
/// prefers a `.dylib` over a `.a` when both are present. Leaving the previous build's
/// artifact behind would let a packaging build silently link the development shared
/// library, or leave a stale archive shadowing nothing at all.
async fn remove_superseded_host_library(
    directory: &Path,
    produced: AppleHostLibrary,
) -> eyre::Result<()> {
    let stale = directory.join(produced.superseded().linked_file_name());
    match fs::remove_file(&stale).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "Failed to remove superseded host library {}",
                stale.display()
            )
        }),
    }
}

/// The features an Apple runtime's generated FFI crate is compiled with.
///
/// Each name is a feature the generated manifest forwards to `waterui-ffi`
/// (`FORWARDED_FFI_FEATURES`), so the resolve stays inside the seeded
/// lockfile. Anything loaded into that runtime has to be compiled with the
/// same set. Cargo
/// unifies features per build and folds the result into the `-C metadata` hash it
/// mangles into every symbol, so a module that enables one feature more or fewer
/// than its host links against a runtime whose symbols no longer match. Both
/// callers derive the set here rather than each listing it, so the two cannot
/// drift apart.
///
/// # Errors
///
/// Returns an error when the project's enabled capabilities cannot be resolved.
pub(crate) async fn apple_ffi_dependency_features(
    project: &Project,
    browser_runtime: BrowserRuntimePlan,
) -> eyre::Result<Vec<String>> {
    let build_manifest = project.ffi_crate_path().join("Cargo.toml");
    let mut features = vec!["c-api".to_string()];
    features.extend(
        crate::project_model::assets::capability_ffi_features(project, &build_manifest).await?,
    );
    if browser_runtime.chromium {
        features.push("chromium".to_string());
    }
    if matches!(browser_runtime.webview, Some(ResolvedWebViewBackend::Cef)) {
        features.push("webview-cef".to_string());
    }
    Ok(features)
}

async fn apple_ffi_build_features(
    project: &Project,
    browser_runtime: BrowserRuntimePlan,
    linkage: RustLinkage,
) -> eyre::Result<Vec<String>> {
    let mut features = apple_ffi_dependency_features(project, browser_runtime).await?;
    if linkage == RustLinkage::SharedRuntime {
        features.push("dev".to_string());
    }
    Ok(features)
}

/// Build Rust library for an Apple platform.
///
/// # Errors
/// Returns an error if the Rust build fails or the expected Apple archive cannot be copied.
pub async fn build_rust_lib(
    project: &Project,
    platform: TargetPlatform,
    options: BuildOptions,
) -> eyre::Result<BuiltTarget> {
    // Resolve fonts BEFORE cargo build - this ensures icons.json is present
    // for crates like fontawesome7 that need it during build.rs
    let font_declarations =
        crate::assets::scan_fonts(project, &project.ffi_crate_path().join("Cargo.toml")).await?;
    let _resolved_fonts = crate::assets::resolve_fonts(font_declarations).await?;
    let browser_runtime_plan = project
        .browser_runtime_plan(platform, TargetBackend::Apple)
        .await?;

    let triple = options
        .target_triple()
        .cloned()
        .unwrap_or_else(|| platform.triple());
    let target = triple.to_string();
    let target_underscore = target.replace('-', "_");
    let host_library = AppleHostLibrary::for_linkage(options.linkage());
    let mut build = RustBuild::new(project.ffi_crate_path(), triple.clone())
        .with_project(project)
        .with_features(
            apple_ffi_build_features(project, browser_runtime_plan, options.linkage()).await?,
        )
        .with_crate_type_override(host_library.crate_type())
        .with_envs(options.cargo_envs().iter().cloned());
    if let Some(sccache_path) = options.sccache_path() {
        build = build.with_sccache(sccache_path.to_path_buf());
    }
    if let Some(progress) = options.progress() {
        build = build.with_progress(progress.clone());
    }
    build = build
        .with_env("PKG_CONFIG_ALLOW_CROSS", "1")
        .with_env(format!("PKG_CONFIG_ALLOW_CROSS_{target_underscore}"), "1")
        .with_env(format!("PKG_CONFIG_ALLOW_CROSS_{target}"), "1");
    let (deployment_environment, deployment_target) =
        apple_deployment_target(project, platform).await?;
    build = build.with_env(deployment_environment, deployment_target);
    if options.linkage() == RustLinkage::SharedRuntime {
        build = build.with_preferred_dynamic_linking();
    }
    build = build.with_target_dir(project.water_target_dir(options.linkage()).await?);
    let built_target = build.build_lib(options.is_release()).await?;

    // Entry-owning packaging installs the ffi crate's own binary as the
    // bundle executable — no Swift host links the library. The loader
    // resolves bundle-local dylibs through both rpaths: `../Frameworks` for
    // the macOS `Contents/` layout, `Frameworks` for the flat layout every
    // other Apple platform uses.
    build
        .clone()
        .with_final_rustc_arg("-Clink-arg=-Wl,-rpath,@executable_path/../Frameworks")
        .with_final_rustc_arg("-Clink-arg=-Wl,-rpath,@executable_path/Frameworks")
        .with_final_rustc_arg("-Clink-arg=-lc++")
        .with_final_rustc_arg("-Clink-arg=-framework")
        .with_final_rustc_arg("-Clink-arg=VideoToolbox")
        .build_binary(APPLE_ENTRY_BINARY_NAME, options.is_release())
        .await?;

    // The helper `[[bin]]` exists only when the manifest declared it — the
    // application's linked engine, not chromium alone — so the build gates
    // on the manifest's own predicate or Cargo reports `no bin target`.
    if project.declares_cef_helper().await? {
        build
            .clone()
            .with_final_rustc_arg("-Clink-arg=-Wl,-rpath,@executable_path/../Frameworks")
            .build_binary(
                &crate::project_model::project_types::cef_helper_binary_name(
                    project.ffi_crate_name().as_str(),
                ),
                options.is_release(),
            )
            .await?;
    }

    // If output_dir is specified, copy the library there
    if let Some(output_dir) = options.output_dir() {
        fs::create_dir_all(output_dir).await?;
        let dest_lib = output_dir.join(host_library.linked_file_name());
        copy_file(&built_target.artifact, &dest_lib).await?;
        remove_superseded_host_library(output_dir, host_library).await?;
        if options.linkage() == RustLinkage::SharedRuntime {
            let libraries = RustDynamicLibraries::resolve(&built_target, &triple, project).await?;
            libraries.stage(output_dir).await?;
            let staged_runtime = libraries.stage_apple_canonical(output_dir).await?;
            if host_library == AppleHostLibrary::Dynamic {
                // The app library records the runtime's cargo-written install
                // name; retarget while the canonical copy still carries it.
                dynamic_runtime::retarget_module(&dest_lib, &staged_runtime).await?;
            }
            dynamic_runtime::prepare_host_runtime(&staged_runtime).await?;
        }
    }

    Ok(built_target)
}

/// The deployment targets the Apple backend supports, as `SEMVER` strings.
///
/// These were `*_DEPLOYMENT_TARGET` build settings in the generated Xcode
/// project; entry-owning packaging has no project file, so they are declared
/// here next to the backend that owns them — the same values `Package.swift`
/// in `apple-backend` publishes.
const fn apple_deployment_target_for(platform: TargetPlatform) -> Option<&'static str> {
    match platform {
        TargetPlatform::MacOS
        | TargetPlatform::IOS
        | TargetPlatform::IOSSimulator
        | TargetPlatform::TvOS
        | TargetPlatform::TvOSSimulator
        | TargetPlatform::WatchOS
        | TargetPlatform::WatchOSSimulator => Some("26.0"),
        TargetPlatform::VisionOS | TargetPlatform::VisionOSSimulator => Some("2.5"),
        _ => None,
    }
}

/// Resolve the deployment-target environment variable an Apple build must carry.
///
/// # Errors
///
/// Returns an error when the platform has no Apple deployment target.
pub async fn apple_deployment_target(
    _project: &Project,
    platform: TargetPlatform,
) -> eyre::Result<(&'static str, String)> {
    let environment = match platform {
        TargetPlatform::MacOS => "MACOSX_DEPLOYMENT_TARGET",
        TargetPlatform::IOS | TargetPlatform::IOSSimulator => "IPHONEOS_DEPLOYMENT_TARGET",
        TargetPlatform::TvOS | TargetPlatform::TvOSSimulator => "TVOS_DEPLOYMENT_TARGET",
        TargetPlatform::WatchOS | TargetPlatform::WatchOSSimulator => "WATCHOS_DEPLOYMENT_TARGET",
        TargetPlatform::VisionOS | TargetPlatform::VisionOSSimulator => "XROS_DEPLOYMENT_TARGET",
        other => {
            bail!("Platform {other:?} does not have an Apple deployment target");
        }
    };
    let target = apple_deployment_target_for(platform).ok_or_else(|| {
        eyre::eyre!("Platform {platform:?} does not have an Apple deployment target")
    })?;
    Ok((environment, target.to_string()))
}

// ============================================================================
// Validation
// ============================================================================

/// The local Apple backend `[backend.apple] backend_path` names is the
/// checkout the generated project references — validate it is a real Swift
/// package. `waterui_path` alone no longer supplies one: the framework
/// checkout carries no `backends/apple` tree since the submodule was dropped.
fn validate_local_apple_backend(project: &Project) -> eyre::Result<()> {
    let Some(backend_path) = project
        .manifest()
        .backends
        .apple()
        .and_then(|backend| backend.backend_path.as_deref())
    else {
        return Ok(());
    };

    let backend_root = {
        let candidate = PathBuf::from(backend_path);
        if candidate.is_absolute() {
            candidate
        } else {
            project.root().join(candidate)
        }
    };

    let package_manifest = backend_root.join("Package.swift");
    if package_manifest.exists() {
        return Ok(());
    }

    bail!(
        "`[backend.apple] backend_path` points at `{}`, which has no `Package.swift` — \
         the Apple backend lives in its own repository now; point it at an \
         `apple-backend` checkout, or remove `backend_path` to consume the pinned \
         release from SwiftPM.",
        backend_root.display()
    );
}

// ============================================================================
// Clean
// ============================================================================

/// Clean build artifacts for an Apple platform.
///
/// Entry-owning packaging keeps only the assembled products directory; the
/// Rust target dir is cleaned by the shared target-dir cache logic.
///
/// # Errors
/// Returns an error when the generated build directories cannot be removed.
pub async fn clean_apple(project: &Project) -> eyre::Result<()> {
    if project.apple_backend().is_none() {
        return Ok(()); // Nothing to clean if no backend configured
    }

    let project_path = project.backend_path::<AppleBackend>();
    for directory in [project_path.join("DerivedData"), project_path.join("build")] {
        if directory.exists() {
            fs::remove_dir_all(&directory).await?;
        }
    }

    Ok(())
}

// ============================================================================
// Package
// ============================================================================

/// Package an Apple app in entry-owning mode.
///
/// The `.app` bundle is assembled directly — the ffi crate's
/// `waterui-apple-main` binary as the executable, resources copied in,
/// `actool` compiling the asset catalog, `codesign` signing — with no Xcode
/// project anywhere in the generated tree.
///
/// # Errors
/// Returns an error if the backend is missing, packaging prerequisites are
/// invalid, or bundle assembly/signing fails.
#[allow(clippy::too_many_lines)]
pub async fn package_apple(
    project: &Project,
    platform: TargetPlatform,
    options: PackageOptions,
    built: &BuiltTarget,
) -> eyre::Result<Artifact> {
    let backend = project
        .apple_backend()
        .ok_or_else(|| eyre::eyre!("Apple backend must be configured"))?;
    let browser_runtime_plan = project
        .browser_runtime_plan(platform, TargetBackend::Apple)
        .await?;

    let project_path = project.backend_path::<AppleBackend>();
    validate_local_apple_backend(project)?;

    let configuration = if options.is_debug() {
        "Debug"
    } else {
        "Release"
    };
    let triple = platform.triple();
    let sdk_name = platform
        .sdk_name()
        .ok_or_else(|| eyre::eyre!("Platform {platform:?} is not an Apple platform"))?;
    let (_, deployment_target) = apple_deployment_target(project, platform).await?;

    // Assets are staged into a scratch directory; the bundle copies them out
    // from there (`waterui_assets`, `fonts`) and `actool` compiles the asset
    // catalog (`WaterUIAssets.xcassets`).
    let staging_dir = project_path.join("DerivedData/AssetStaging");
    copy_assets_and_fonts(
        project,
        &staging_dir,
        &built.app_symbols()?,
        options.uses_dev_server(),
    )
    .await?;

    // Xcode used "Debug-iphonesimulator"-style product configuration names;
    // the same layout keeps `simctl`/`devicectl` installs pointed at a stable
    // location.
    let products_config = if sdk_name == "macosx" {
        configuration.to_string()
    } else {
        format!("{configuration}-{sdk_name}")
    };
    let products_dir = project_path
        .join("DerivedData")
        .join("Build/Products")
        .join(&products_config);
    let product_name = crate::apple::backend::apple_product_name(project)?.to_string();
    let app_path = products_dir.join(format!("{product_name}.app"));

    #[cfg(target_os = "macos")]
    if platform == TargetPlatform::MacOS {
        browser_runtime::remove_macos_app(&app_path.join("Contents")).await?;
        remove_cef_helper_apps(&app_path, &product_name).await?;
    }

    let ctx = AppleBackend::template_context(project).await?;
    let layout = app_bundle::AppleAppLayout::for_app(&app_path, sdk_name);

    let executable = built.profile_dir.join(APPLE_ENTRY_BINARY_NAME);
    let info_plist = app_bundle::apple_info_plist(
        &ctx,
        project,
        platform,
        &deployment_target,
        &product_name,
        project.bundle_identifier(),
    );

    app_bundle::assemble_app_bundle(
        &layout,
        &executable,
        &product_name,
        &staging_dir,
        &info_plist,
        sdk_name,
        &deployment_target,
    )
    .await?;

    // The shared-runtime development linkage ships `libwaterui_dylib` and the
    // Rust standard library inside the bundle's Frameworks directory; a
    // statically linked package carries neither.
    let shared_runtime = if options.uses_shared_rust_runtime() {
        let bin_built = BuiltTarget {
            profile_dir: built.profile_dir.clone(),
            artifact: layout.executable_file(&product_name),
            shared_runtime: built.shared_runtime.clone(),
            app_library: None,
        };
        let libraries = RustDynamicLibraries::resolve(&bin_built, &triple, project).await?;
        libraries.stage(&layout.frameworks_dir).await?;
        let staged_runtime = libraries
            .stage_apple_canonical(&layout.frameworks_dir)
            .await?;
        // Redirect the executable's recorded runtime dependency to the
        // canonical `@rpath` name, the way the Swift host library was
        // retargeted before.
        dynamic_runtime::retarget_module(&layout.executable_file(&product_name), &staged_runtime)
            .await?;
        dynamic_runtime::prepare_host_runtime(&staged_runtime).await?;
        Some(libraries)
    } else {
        RustDynamicLibraries::remove_staged(&layout.frameworks_dir, &triple).await?;
        None
    };
    let _ = shared_runtime;

    #[cfg(target_os = "macos")]
    if platform == TargetPlatform::MacOS && browser_runtime_plan.requires_cef() {
        browser_runtime::stage_macos_app(
            browser_runtime_plan,
            &built.profile_dir,
            &app_path.join("Contents"),
        )
        .await?;
        // Helper bundles wrap the helper `[[bin]]`, which the manifest
        // declares only when the application links the CEF engine crate —
        // chromium alone stages the runtime but builds no helper.
        if project.declares_cef_helper().await? {
            let main_binary = layout.executable_file(&product_name);
            let helper_binary = built.profile_dir.join(
                crate::project_model::project_types::cef_helper_binary_name(
                    project.ffi_crate_name().as_str(),
                ),
            );
            package_cef_helper_app(
                &app_path,
                &main_binary,
                &helper_binary,
                project.bundle_identifier(),
            )
            .await?;
        }
    }

    app_bundle::sign_apple_app(
        &layout,
        platform,
        &options,
        backend,
        project_path.as_path(),
        project,
    )
    .await?;

    Ok(Artifact::new(project.bundle_identifier(), app_path))
}

// ============================================================================
// Asset and Font Handling
// ============================================================================

/// Copy project assets and dependency fonts to the app resources directory.
/// `symbols` is the app library artifact the target build produced, whose
/// `waterui_meta_bundle_*` statics declare the asset mounts.
async fn copy_assets_and_fonts(
    project: &Project,
    dest_dir: &Path,
    symbols: &crate::artifact_symbols::ArtifactSymbols,
    dev_server: bool,
) -> eyre::Result<()> {
    // Stage project assets using platform-native conventions.
    let manifest =
        assets::stage_project_assets_for_apple(project, dest_dir, symbols, dev_server).await?;

    // Scan and resolve dependency fonts
    let font_declarations =
        assets::scan_fonts(project, &project.ffi_crate_path().join("Cargo.toml")).await?;
    let mut resolved_fonts = assets::resolve_fonts(font_declarations).await?;
    resolved_fonts.extend(assets::scan_project_font_assets(&manifest)?);

    if !resolved_fonts.is_empty() {
        // Copy fonts to app resources; `waterui-apple` registers every font
        // file in the bundle at startup.
        let fonts_dest = dest_dir.join("fonts");
        assets::copy_fonts(&resolved_fonts, &fonts_dest).await?;

        info!("Copied {} fonts to Apple app", resolved_fonts.len());
    }

    Ok(())
}

// ============================================================================
// Platform Support Check
// ============================================================================

/// Check if a platform is supported by the Apple backend.
#[must_use]
pub const fn is_apple_platform(platform: TargetPlatform) -> bool {
    matches!(
        platform,
        TargetPlatform::MacOS
            | TargetPlatform::IOS
            | TargetPlatform::IOSSimulator
            | TargetPlatform::TvOS
            | TargetPlatform::TvOSSimulator
            | TargetPlatform::WatchOS
            | TargetPlatform::WatchOSSimulator
            | TargetPlatform::VisionOS
            | TargetPlatform::VisionOSSimulator
    )
}
