//! `water package` command implementation.

use std::path::PathBuf;

use clap::{Args as ClapArgs, ValueEnum};
use eyre::{Result, bail};

use crate::shell::Shell;
use crate::{header, success};
use waterui_cli::toolchain_checks;
use waterui_cli::{
    android::platform::{AndroidAbi, AndroidPlatform},
    apple::platform::{build_rust_lib, package_apple},
    apple::toolchain::AppleSdk,
    backend::reinit_backend,
    build::{BuildOptions, BuildProfile, BuiltTarget, stage_dxc_runtime},
    device::Artifact,
    gtk4::{
        backend::Gtk4Backend,
        platform::{build_gtk4, package_gtk4},
    },
    hydrolysis::{
        backend::HydrolysisBackend,
        platform::{build_hydrolysis, package_hydrolysis},
    },
    package_output::place_in_project,
    platform::{PackageOptions, TargetPlatform as LibTargetPlatform},
    project::{ManagedBackends, Project},
    winui::{
        backend::WinUiBackend,
        platform::{build_winui, package_winui},
    },
};

/// Target platform for packaging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TargetPlatform {
    /// iOS (physical device).
    Ios,
    /// iOS Simulator.
    IosSimulator,
    /// Android.
    Android,
    /// macOS.
    Macos,
    /// Linux.
    Linux,
    /// Windows.
    Windows,
    /// Web.
    Web,
}

/// Target backend for packaging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TargetBackend {
    /// Apple backend (UIKit/AppKit).
    Apple,
    /// Android backend.
    Android,
    /// GTK4 backend.
    Gtk4,
    /// Hydrolysis backend.
    Hydrolysis,
    /// `WinUI` backend.
    #[value(name = "winui")]
    WinUi,
}

impl TargetBackend {
    /// Whether the backend is experimental — shipped without full testing
    /// ahead of milestone releases — so selecting it asks for confirmation.
    const fn is_experimental(self) -> bool {
        matches!(self, Self::Gtk4 | Self::WinUi)
    }
}

/// Target architecture for Android builds.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AndroidArch {
    /// ARM64 (arm64-v8a) - modern Android devices
    Arm64,
    /// `x86_64` - emulators on Intel/AMD
    X86_64,
    /// `ARMv7` (armeabi-v7a) - older 32-bit devices
    Armv7,
    /// x86 - older 32-bit emulators
    X86,
}

impl AndroidArch {
    /// Convert to Android ABI string.
    const fn to_abi(self) -> AndroidAbi {
        match self {
            Self::Arm64 => AndroidAbi::Arm64V8a,
            Self::X86_64 => AndroidAbi::X86_64,
            Self::Armv7 => AndroidAbi::ArmeabiV7a,
            Self::X86 => AndroidAbi::X86,
        }
    }
}

/// Arguments for the package command.
#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Target platform to package for.
    #[arg(short, long, value_enum)]
    platform: TargetPlatform,

    /// Backend to use (overrides default for platform).
    /// Required: `water package` always needs an explicit backend.
    #[arg(short, long, value_enum)]
    backend: TargetBackend,

    /// Build in release mode (optimized).
    #[arg(long)]
    release: bool,

    /// Package for store distribution (App Store, Play Store).
    #[arg(long)]
    distribution: bool,

    /// Project directory path (defaults to current directory).
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Target architectures for Android (comma-separated).
    /// Examples: --arch arm64, --arch `arm64,x86-64`
    /// Required when packaging Android backend.
    #[arg(long, value_enum, value_delimiter = ',')]
    arch: Vec<AndroidArch>,

    /// Skip the confirmation prompt required by experimental backends
    /// (needed in non-interactive environments).
    #[arg(short = 'y', long)]
    yes: bool,
}

struct PackagingContext {
    project: Project,
    backend: TargetBackend,
    build_options: BuildOptions,
}

/// Run the package command.
pub async fn run(shell: &Shell, args: Args) -> Result<()> {
    // The packaging context carries the opened project, the resolved backend and
    // the build options; on Windows that future crosses clippy's `large_futures`
    // threshold (16 KiB), so it is pinned on the heap instead of the caller's stack.
    let Some(context) = Box::pin(prepare_packaging_context(shell, &args)).await? else {
        return Ok(());
    };
    print_packaging_header(
        shell,
        &context.project,
        args.platform,
        context.backend,
        args.release,
        args.distribution,
    );
    check_packaging_toolchain(shell, args.platform, context.backend, &args.arch).await?;
    let built = build_packaging_artifacts(shell, &args, &context).await?;
    package_artifact(shell, &args, &context, built.as_ref()).await
}

async fn prepare_packaging_context(shell: &Shell, args: &Args) -> Result<Option<PackagingContext>> {
    let project_path = crate::project_path::canonicalize(&args.path)?;
    let managed_backends = ManagedBackends::for_platform(lib_platform(args.platform));
    let project = Project::open(&project_path, managed_backends).await?;
    let backend = resolve_backend(args.platform, args.backend)?;

    validate_arch_args(backend, &args.arch)?;
    validate_desktop_backend_platform_on_host(args.platform, backend)?;
    ensure_packaging_backend_ready(&project, backend)?;

    if backend.is_experimental()
        && !super::confirm_experimental_backend(shell, backend_name(backend), args.yes)?
    {
        return Ok(None);
    }
    let project = ensure_packaging_backend_generated(shell, project, backend).await?;

    let mut build_options = BuildOptions::packaging(if args.release {
        BuildProfile::Release
    } else {
        BuildProfile::Debug
    })
    .with_progress(shell.build_progress());
    if let Some(sccache_path) =
        super::detect_sccache_path(shell, &waterui_cli::toolchain::Host::current()).await
    {
        build_options = build_options.with_sccache(sccache_path);
    }

    Ok(Some(PackagingContext {
        project,
        backend,
        build_options,
    }))
}

fn ensure_packaging_backend_ready(project: &Project, backend: TargetBackend) -> Result<()> {
    if project.is_playground() {
        return Ok(());
    }

    match backend {
        TargetBackend::Apple if project.apple_backend().is_none() => {
            bail!("Apple backend is not configured. Run `water backend add apple`.");
        }
        TargetBackend::Android if project.android_backend().is_none() => {
            bail!("Android backend is not configured. Run `water backend add android`.");
        }
        TargetBackend::Gtk4 if project.gtk4_backend().is_none() => {
            bail!("GTK4 backend is not configured. Run `water backend add gtk4`.");
        }
        TargetBackend::Hydrolysis if project.hydrolysis_backend().is_none() => {
            bail!("Hydrolysis backend is not configured. Run `water backend add hydrolysis`.");
        }
        TargetBackend::WinUi if project.winui_backend().is_none() => {
            bail!("WinUI backend is not configured. Run `water backend add winui`.");
        }
        _ => Ok(()),
    }
}

async fn ensure_packaging_backend_generated(
    shell: &Shell,
    project: Project,
    backend: TargetBackend,
) -> Result<Project> {
    match backend {
        TargetBackend::Gtk4 if project.is_playground() => {
            let needs_reinit = Gtk4Backend::requires_regeneration(&project).await?;
            ensure_packaging_generated_backend::<Gtk4Backend>(
                shell,
                project,
                needs_reinit,
                "Initializing GTK4 backend...",
                "GTK4 backend initialized",
            )
            .await
        }
        TargetBackend::Hydrolysis if project.is_playground() => {
            let needs_reinit = HydrolysisBackend::requires_regeneration(&project).await?;
            ensure_packaging_generated_backend::<HydrolysisBackend>(
                shell,
                project,
                needs_reinit,
                "Initializing hydrolysis backend...",
                "Hydrolysis backend initialized",
            )
            .await
        }
        TargetBackend::WinUi if project.is_playground() => {
            let needs_reinit = WinUiBackend::requires_regeneration(&project).await?;
            ensure_packaging_generated_backend::<WinUiBackend>(
                shell,
                project,
                needs_reinit,
                "Initializing WinUI backend...",
                "WinUI backend initialized",
            )
            .await
        }
        _ => Ok(project),
    }
}

async fn ensure_packaging_generated_backend<T>(
    shell: &Shell,
    project: Project,
    needs_reinit: bool,
    spinner_message: &str,
    success_message: &str,
) -> Result<Project>
where
    T: waterui_cli::backend::Backend,
{
    if !needs_reinit {
        return Ok(project);
    }

    let spinner = shell.spinner(spinner_message);
    reinit_backend::<T>(&project).await?;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    success!(shell, "{success_message}");
    Ok(project)
}

fn print_packaging_header(
    shell: &Shell,
    project: &Project,
    platform: TargetPlatform,
    backend: TargetBackend,
    release: bool,
    distribution: bool,
) {
    let mode = if release { "release" } else { "debug" };
    let dist = if distribution { " (distribution)" } else { "" };
    header!(
        shell,
        "Packaging {} for {} via {} ({}){}",
        project.crate_name(),
        platform_name(platform),
        backend_name(backend),
        mode,
        dist
    );
}

async fn check_packaging_toolchain(
    shell: &Shell,
    platform: TargetPlatform,
    backend: TargetBackend,
    arch: &[AndroidArch],
) -> Result<()> {
    let spinner = shell.spinner("Checking toolchain...");
    check_toolchain_for_backend(
        &waterui_cli::toolchain::Host::current(),
        platform,
        backend,
        arch,
    )
    .await?;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    success!(shell, "Toolchain ready");
    Ok(())
}

async fn build_packaging_artifacts(
    shell: &Shell,
    args: &Args,
    context: &PackagingContext,
) -> Result<Option<BuiltTarget>> {
    match context.backend {
        TargetBackend::Android => {
            build_android_packaging_artifacts(
                shell,
                &context.project,
                &args.arch,
                context.build_options.clone(),
            )
            .await
        }
        TargetBackend::Apple => {
            build_apple_packaging_artifacts(
                shell,
                &context.project,
                args.platform,
                context.build_options.clone(),
            )
            .await
        }
        TargetBackend::Gtk4 => {
            build_gtk4_packaging_artifacts(shell, &context.project, context.build_options.clone())
                .await
        }
        TargetBackend::Hydrolysis => {
            build_hydrolysis_packaging_artifacts(
                shell,
                &context.project,
                args.platform,
                context.build_options.clone(),
            )
            .await
        }
        TargetBackend::WinUi => {
            build_winui_packaging_artifacts(shell, &context.project, context.build_options.clone())
                .await
        }
    }
}

async fn build_android_packaging_artifacts(
    shell: &Shell,
    project: &Project,
    arch: &[AndroidArch],
    build_options: BuildOptions,
) -> Result<Option<BuiltTarget>> {
    let mut built = None;
    AndroidPlatform::clean_jni_libs(project).await?;
    for arch in arch {
        let abi = arch.to_abi();
        let spinner = shell.spinner(format!("Building Rust library ({})...", abi.as_str()));
        let target = shell
            .display_output(AndroidPlatform::new(abi).build(project, build_options.clone()))
            .await?;
        built = Some(target);
        if let Some(pb) = spinner {
            pb.finish_and_clear();
        }
        success!(shell, "Built for {}", abi.as_str());
    }
    Ok(built)
}

async fn build_apple_packaging_artifacts(
    shell: &Shell,
    project: &Project,
    platform: TargetPlatform,
    build_options: BuildOptions,
) -> Result<Option<BuiltTarget>> {
    let spinner = shell.spinner("Building Rust library...");
    let built = shell
        .display_output(build_rust_lib(
            project,
            lib_platform(platform),
            build_options,
        ))
        .await?;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    success!(shell, "Built Rust library");
    Ok(Some(built))
}

async fn build_gtk4_packaging_artifacts(
    shell: &Shell,
    project: &Project,
    build_options: BuildOptions,
) -> Result<Option<BuiltTarget>> {
    let spinner = shell.spinner("Building GTK4 app...");
    let built = shell
        .display_output(build_gtk4(project, build_options))
        .await?;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    success!(shell, "Built GTK4 app");
    Ok(Some(built))
}

async fn build_winui_packaging_artifacts(
    shell: &Shell,
    project: &Project,
    build_options: BuildOptions,
) -> Result<Option<BuiltTarget>> {
    let spinner = shell.spinner("Building WinUI app...");
    let built = shell
        .display_output(build_winui(project, build_options))
        .await?;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    success!(shell, "Built WinUI app");
    Ok(Some(built))
}

async fn build_hydrolysis_packaging_artifacts(
    shell: &Shell,
    project: &Project,
    platform: TargetPlatform,
    build_options: BuildOptions,
) -> Result<Option<BuiltTarget>> {
    if platform == TargetPlatform::Web {
        return Ok(None);
    }

    let spinner = shell.spinner("Building hydrolysis app...");
    let built = shell
        .display_output(build_hydrolysis(
            project,
            hydrolysis_platform(platform),
            build_options,
        ))
        .await?;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    success!(shell, "Built hydrolysis app");
    Ok(Some(built))
}

async fn package_artifact(
    shell: &Shell,
    args: &Args,
    context: &PackagingContext,
    built: Option<&BuiltTarget>,
) -> Result<()> {
    let spinner = shell.spinner("Packaging application...");
    let artifact = shell
        .display_output(package_artifact_inner(shell, args, context, built))
        .await?;
    let artifact = place_in_project(&context.project, artifact).await?;
    // A packaged Windows hydrolysis binary is statically linked, but wgpu's
    // DirectX 12 backend still `LoadLibrary`s `dxcompiler.dll` and `dxil.dll`
    // by name at run time; the pair has to ship beside the artifact.
    if cfg!(windows)
        && context.backend == TargetBackend::Hydrolysis
        && args.platform == TargetPlatform::Windows
    {
        let destination = artifact.path().parent().ok_or_else(|| {
            eyre::eyre!(
                "packaged artifact {} has no parent directory",
                artifact.path().display()
            )
        })?;
        stage_dxc_runtime(destination).await?;
    }
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    success!(shell, "Packaged at {}", artifact.path().display());
    Ok(())
}

async fn package_artifact_inner(
    shell: &Shell,
    args: &Args,
    context: &PackagingContext,
    built: Option<&BuiltTarget>,
) -> Result<Artifact> {
    let package_options = PackageOptions::packaging(args.distribution, !args.release)
        .with_progress(shell.build_progress());
    match context.backend {
        TargetBackend::Android => {
            let abis: Vec<AndroidAbi> = args.arch.iter().map(|arch| arch.to_abi()).collect();
            AndroidPlatform::package_with_abis(&context.project, package_options, &abis).await
        }
        TargetBackend::Apple => {
            let built = built.ok_or_else(|| {
                eyre::eyre!("Internal error: Apple packaging has no build result")
            })?;
            package_apple(
                &context.project,
                lib_platform(args.platform),
                package_options,
                built,
            )
            .await
        }
        TargetBackend::Gtk4 => {
            package_gtk4(
                &context.project,
                package_options,
                built.ok_or_else(|| {
                    eyre::eyre!("Internal error: GTK4 packaging has no build result")
                })?,
            )
            .await
        }
        TargetBackend::WinUi => {
            package_winui(
                &context.project,
                package_options,
                built.ok_or_else(|| {
                    eyre::eyre!("Internal error: WinUI packaging has no build result")
                })?,
            )
            .await
        }
        TargetBackend::Hydrolysis => {
            package_hydrolysis(
                &context.project,
                hydrolysis_platform(args.platform),
                package_options,
                built,
            )
            .await
        }
    }
}

fn resolve_backend(platform: TargetPlatform, backend: TargetBackend) -> Result<TargetBackend> {
    let supported = matches!(
        (platform, backend),
        (
            TargetPlatform::Ios | TargetPlatform::IosSimulator,
            TargetBackend::Apple
        ) | (
            TargetPlatform::Macos,
            TargetBackend::Apple | TargetBackend::Hydrolysis
        ) | (TargetPlatform::Android, TargetBackend::Android)
            | (
                TargetPlatform::Linux,
                TargetBackend::Gtk4 | TargetBackend::Hydrolysis
            )
            | (
                TargetPlatform::Windows,
                TargetBackend::Hydrolysis | TargetBackend::WinUi
            )
            | (TargetPlatform::Web, TargetBackend::Hydrolysis)
    );

    if !supported {
        bail!(
            "Backend {:?} does not support platform {:?}.\n\
             Valid combinations:\n  \
             - iOS/iOS Simulator: apple\n  \
             - Android: android\n  \
             - macOS: apple, hydrolysis\n  \
             - Linux: gtk4, hydrolysis\n  \
             - Windows: hydrolysis, winui\n  \
             - Web: hydrolysis",
            backend,
            platform
        );
    }

    Ok(backend)
}

fn validate_arch_args(backend: TargetBackend, arch: &[AndroidArch]) -> Result<()> {
    if backend == TargetBackend::Android && arch.is_empty() {
        bail!(
            "Android backend requires --arch.\n\
             Examples:\n  \
             water package --platform android --backend android --arch arm64\n  \
             water package --platform android --backend android --arch arm64,x86-64"
        );
    }

    if backend != TargetBackend::Android && !arch.is_empty() {
        bail!("--arch is only valid when packaging Android backend");
    }

    Ok(())
}

async fn check_toolchain_for_backend(
    host: &waterui_cli::toolchain::Host,
    platform: TargetPlatform,
    backend: TargetBackend,
    arch: &[AndroidArch],
) -> Result<()> {
    match backend {
        TargetBackend::Apple => {
            let sdk = match platform {
                TargetPlatform::Ios => AppleSdk::Ios,
                TargetPlatform::IosSimulator => AppleSdk::IosSimulator,
                TargetPlatform::Macos => AppleSdk::Macos,
                TargetPlatform::Android
                | TargetPlatform::Linux
                | TargetPlatform::Windows
                | TargetPlatform::Web => {
                    bail!("Internal error: Apple backend is not supported on {platform:?}");
                }
            };
            toolchain_checks::check_apple(host, sdk).await?;
        }
        TargetBackend::Android => {
            if platform != TargetPlatform::Android {
                bail!("Internal error: Android backend is not supported on {platform:?}");
            }
            let required_abis = arch.iter().map(|arch| arch.to_abi()).collect::<Vec<_>>();
            toolchain_checks::check_android_build_or_package_for_abis(host, &required_abis).await?;
        }
        TargetBackend::Gtk4 => {
            if platform != TargetPlatform::Linux {
                bail!("Internal error: GTK4 backend is not supported on {platform:?}");
            }
            toolchain_checks::check_gtk4(host).await?;
        }
        TargetBackend::Hydrolysis => {
            if platform != TargetPlatform::Macos
                && platform != TargetPlatform::Linux
                && platform != TargetPlatform::Windows
                && platform != TargetPlatform::Web
            {
                bail!("Internal error: hydrolysis backend is not supported on {platform:?}");
            }
            if platform == TargetPlatform::Web {
                toolchain_checks::check_web(host).await?;
            } else {
                toolchain_checks::check_hydrolysis(host).await?;
            }
        }
        TargetBackend::WinUi => {
            if platform != TargetPlatform::Windows {
                bail!("Internal error: WinUI backend is not supported on {platform:?}");
            }
            toolchain_checks::check_winui(host).await?;
        }
    }
    Ok(())
}

fn validate_desktop_backend_platform_on_host(
    platform: TargetPlatform,
    backend: TargetBackend,
) -> Result<()> {
    if platform == TargetPlatform::Web {
        return Ok(());
    }

    match backend {
        TargetBackend::Gtk4 => {
            #[cfg(target_os = "linux")]
            {
                if platform != TargetPlatform::Linux {
                    bail!("GTK4 backend on Linux host requires --platform linux");
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                bail!("GTK4 backend is only supported on Linux hosts");
            }
        }
        TargetBackend::Hydrolysis => {
            #[cfg(target_os = "macos")]
            if platform != TargetPlatform::Macos {
                bail!("Hydrolysis backend on macOS host requires --platform macos");
            }

            #[cfg(target_os = "linux")]
            if platform != TargetPlatform::Linux {
                bail!("Hydrolysis backend on Linux host requires --platform linux");
            }

            #[cfg(target_os = "windows")]
            if platform != TargetPlatform::Windows {
                bail!("Hydrolysis backend on Windows host requires --platform windows");
            }

            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
            bail!("Hydrolysis backend is only supported on macOS, Linux, or Windows hosts");
        }
        TargetBackend::WinUi => {
            #[cfg(target_os = "windows")]
            if platform != TargetPlatform::Windows {
                bail!("WinUI backend on Windows host requires --platform windows");
            }

            #[cfg(not(target_os = "windows"))]
            bail!("WinUI backend is only supported on Windows hosts");
        }
        TargetBackend::Apple => {
            #[cfg(not(target_os = "macos"))]
            bail!("Apple backend requires a macOS host");
        }
        TargetBackend::Android => {}
    }

    Ok(())
}

const fn lib_platform(platform: TargetPlatform) -> LibTargetPlatform {
    match platform {
        TargetPlatform::Ios => LibTargetPlatform::IOS,
        TargetPlatform::IosSimulator => LibTargetPlatform::IOSSimulator,
        TargetPlatform::Android => LibTargetPlatform::Android,
        TargetPlatform::Macos => LibTargetPlatform::MacOS,
        TargetPlatform::Linux => LibTargetPlatform::Linux,
        TargetPlatform::Windows => LibTargetPlatform::Windows,
        TargetPlatform::Web => LibTargetPlatform::Web,
    }
}

const fn hydrolysis_platform(platform: TargetPlatform) -> LibTargetPlatform {
    match platform {
        TargetPlatform::Macos => LibTargetPlatform::MacOS,
        TargetPlatform::Linux => LibTargetPlatform::Linux,
        TargetPlatform::Windows => LibTargetPlatform::Windows,
        TargetPlatform::Web => LibTargetPlatform::Web,
        TargetPlatform::Ios | TargetPlatform::IosSimulator | TargetPlatform::Android => {
            panic!("unsupported hydrolysis platform")
        }
    }
}

const fn platform_name(platform: TargetPlatform) -> &'static str {
    match platform {
        TargetPlatform::Ios => "iOS",
        TargetPlatform::IosSimulator => "iOS Simulator",
        TargetPlatform::Android => "Android",
        TargetPlatform::Macos => "macOS",
        TargetPlatform::Linux => "Linux",
        TargetPlatform::Windows => "Windows",
        TargetPlatform::Web => "Web",
    }
}

const fn backend_name(backend: TargetBackend) -> &'static str {
    match backend {
        TargetBackend::Apple => "Apple",
        TargetBackend::Android => "Android",
        TargetBackend::Gtk4 => "GTK4",
        TargetBackend::Hydrolysis => "Hydrolysis",
        TargetBackend::WinUi => "WinUI",
    }
}

#[cfg(test)]
mod tests {
    use super::{AndroidArch, TargetBackend, TargetPlatform, resolve_backend, validate_arch_args};

    #[test]
    fn only_gtk4_and_winui_are_experimental() {
        use clap::ValueEnum;
        for backend in TargetBackend::value_variants() {
            assert_eq!(
                backend.is_experimental(),
                matches!(backend, TargetBackend::Gtk4 | TargetBackend::WinUi),
                "{backend:?} experimental flag drifted"
            );
        }
    }

    #[test]
    fn rejects_empty_arch_for_android_backend() {
        assert!(validate_arch_args(TargetBackend::Android, &[]).is_err());
    }

    #[test]
    fn rejects_arch_for_non_android_backend() {
        let err = validate_arch_args(TargetBackend::Apple, &[AndroidArch::Arm64])
            .expect_err("non-android --arch should fail");
        assert!(err.to_string().contains("--arch is only valid"));
    }

    #[test]
    fn accepts_android_arch_values() {
        assert!(validate_arch_args(TargetBackend::Android, &[AndroidArch::Arm64]).is_ok());
    }

    #[test]
    fn resolve_backend_validates_explicit_backend() {
        assert_eq!(
            resolve_backend(TargetPlatform::Android, TargetBackend::Android)
                .expect("android backend"),
            TargetBackend::Android
        );
        assert_eq!(
            resolve_backend(TargetPlatform::Windows, TargetBackend::Hydrolysis)
                .expect("windows backend"),
            TargetBackend::Hydrolysis
        );
        assert!(resolve_backend(TargetPlatform::Windows, TargetBackend::Gtk4).is_err());
        assert_eq!(
            resolve_backend(TargetPlatform::Windows, TargetBackend::WinUi)
                .expect("windows winui backend"),
            TargetBackend::WinUi
        );
        assert!(resolve_backend(TargetPlatform::Linux, TargetBackend::WinUi).is_err());
        assert_eq!(
            resolve_backend(TargetPlatform::Web, TargetBackend::Hydrolysis).expect("web backend"),
            TargetBackend::Hydrolysis
        );
    }
}
