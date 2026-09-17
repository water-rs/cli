//! Toolchain diagnostics for the `water doctor` command.
//!
//! [`doctor`] runs every check against an explicit [`Host`], so the report is
//! fully determined by that host's environment, PATH, and filesystem — never
//! by ambient process state. Each [`DoctorItem`] carries a stable
//! machine-readable `id` (`DoctorItem::id`) for `--json` output and tests.

use std::borrow::Cow;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use semver::Version;

use crate::{
    android::{
        device::AndroidDevice,
        platform::AndroidPlatform,
        toolchain::{
            AndroidBuildTools, AndroidNdk, AndroidPlatformTools, AndroidRustTargets, AndroidSdk,
            AndroidSdkPlatforms, Java, Kotlin,
        },
    },
    apple::{
        device::AppleSimulator,
        toolchain::{AppleSdk, Xcode},
    },
    device::Device,
    esp32::{chip::Esp32Chip, toolchain::Esp32Toolchain},
    framework::manifest_rust_version,
    gtk4::toolchain::Gtk4Toolchain,
    platform::TargetPlatform,
    project::{Manifest, PackageType},
    toolchain::{
        Host, Installation, Toolchain, ToolchainError, UnfixableToolchain,
        cargo_helpers::CargoHelpers,
        cmake::Cmake,
        linux::LinuxSystemToolchain,
        rust::{CLI_MINIMUM_RUST_VERSION, RustToolchain},
        sccache::Sccache,
        web::{PackageManagerToolchain, WasmPack, wasm32_target},
        windows_arm64_llvm::WindowsArm64LlvmToolchain,
    },
    utils::parse_semver_version,
    winui::toolchain::WinUiToolchain,
};
use serde::{Deserialize, Serialize};

/// Status of a toolchain check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Toolchain is available and working.
    Ok,
    /// Toolchain is missing or misconfigured.
    Missing,
    /// Toolchain check was skipped (e.g., not applicable on this platform).
    Skipped,
}

impl CheckStatus {
    /// The stable `snake_case` label emitted in `--json` records.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Skipped => "skipped",
        }
    }
}

/// A boxed async function that performs an installation.
pub type BoxedInstallFn =
    Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> + Send>;

/// A single item in the doctor report.
pub struct DoctorItem {
    /// Stable machine-readable identifier (e.g. `android-sdk`).
    pub id: &'static str,
    /// Human-readable name of the toolchain or component.
    pub name: &'static str,
    /// Status of the check.
    pub status: CheckStatus,
    /// Optional message with details or suggestions.
    pub message: Option<String>,
    /// Optional installation function if the issue can be fixed automatically.
    pub install_fn: Option<BoxedInstallFn>,
}

impl std::fmt::Debug for DoctorItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoctorItem")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("status", &self.status)
            .field("message", &self.message)
            .field("install_fn", &self.install_fn.as_ref().map(|_| "..."))
            .finish()
    }
}

/// The JSON record emitted for each [`DoctorItem`] by `water doctor --json`.
///
/// Lives in the library (not the shell) so integration tests deserialize the
/// binary's stdout with the same schema the command serializes. Fields are
/// `Cow` so serialization borrows the static strings while deserialization
/// (the `--json` smoke test) produces owned values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorItemRecord {
    /// Record discriminator, like the shell's other typed records.
    #[serde(rename = "type")]
    pub ty: Cow<'static, str>,
    /// Stable machine-readable item identifier.
    pub id: Cow<'static, str>,
    /// Human-readable item name.
    pub name: Cow<'static, str>,
    /// `ok`, `missing`, or `skipped`.
    pub status: Cow<'static, str>,
    /// Whether `--fix` can remediate the item automatically.
    pub fixable: bool,
    /// Detail or suggestion shown to the user, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<&DoctorItem> for DoctorItemRecord {
    fn from(item: &DoctorItem) -> Self {
        Self {
            ty: Cow::Borrowed("doctor-item"),
            id: Cow::Borrowed(item.id),
            name: Cow::Borrowed(item.name),
            status: Cow::Borrowed(item.status.as_str()),
            fixable: item.is_fixable(),
            message: item.message.clone(),
        }
    }
}

impl DoctorItem {
    const fn ok(id: &'static str, name: &'static str) -> Self {
        Self {
            id,
            name,
            status: CheckStatus::Ok,
            message: None,
            install_fn: None,
        }
    }

    fn missing(id: &'static str, name: &'static str, message: impl Into<String>) -> Self {
        Self {
            id,
            name,
            status: CheckStatus::Missing,
            message: Some(message.into()),
            install_fn: None,
        }
    }

    fn fixable<I: Installation + Send + 'static>(
        id: &'static str,
        name: &'static str,
        message: impl Into<String>,
        installation: I,
        host: &Host,
    ) -> Self {
        let host = host.clone();
        Self {
            id,
            name,
            status: CheckStatus::Missing,
            message: Some(message.into()),
            install_fn: Some(Box::new(move || {
                Box::pin(async move { installation.install(&host).await.map_err(Into::into) })
            })),
        }
    }

    const fn skipped(id: &'static str, name: &'static str) -> Self {
        Self {
            id,
            name,
            status: CheckStatus::Skipped,
            message: None,
            install_fn: None,
        }
    }

    fn skipped_with_message(
        id: &'static str,
        name: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name,
            status: CheckStatus::Skipped,
            message: Some(message.into()),
            install_fn: None,
        }
    }

    /// Returns `true` if the issue can be fixed automatically.
    #[must_use]
    pub const fn is_fixable(&self) -> bool {
        self.install_fn.is_some()
    }
}

/// Stable identifiers for every item in the doctor report.
///
/// These are the contract asserted by `water doctor --json` consumers and the
/// integration test; renaming one is a breaking change to that stream.
pub mod ids {
    /// `xcodebuild`/`xcode-select` presence.
    pub const XCODE: &str = "xcode";
    /// iOS device SDK via `xcrun --sdk iphoneos`.
    pub const IOS_SDK: &str = "ios-sdk";
    /// iOS simulator SDK via `xcrun --sdk iphonesimulator`.
    pub const IOS_SIMULATOR_SDK: &str = "ios-simulator-sdk";
    /// At least one iOS simulator runtime/device.
    pub const IOS_SIMULATORS: &str = "ios-simulators";
    /// macOS SDK via `xcrun --sdk macosx`.
    pub const MACOS_SDK: &str = "macos-sdk";
    /// rustup-managed Rust toolchain, version floor, and host target.
    pub const RUST: &str = "rust";
    /// iOS device and simulator rustup targets on the selected toolchain.
    pub const APPLE_RUST_TARGETS: &str = "apple-rust-targets";
    /// Android SDK root + `sdkmanager`.
    pub const ANDROID_SDK: &str = "android-sdk";
    /// `platform-tools` (`adb`).
    pub const ANDROID_PLATFORM_TOOLS: &str = "android-platform-tools";
    /// `platforms;android-*` packages (`android.jar`).
    pub const ANDROID_SDK_PLATFORMS: &str = "android-sdk-platforms";
    /// `build-tools;*` packages (`d8`).
    pub const ANDROID_BUILD_TOOLS: &str = "android-build-tools";
    /// Android NDK + host clang.
    pub const ANDROID_NDK: &str = "android-ndk";
    /// rustup Android targets for the configured ABIs.
    pub const ANDROID_RUST_TARGETS: &str = "android-rust-targets";
    /// A connected device or an emulator AVD to run on.
    pub const ANDROID_RUN_TARGETS: &str = "android-run-targets";
    /// Host `cmake`.
    pub const CMAKE: &str = "cmake";
    /// LLVM `clang-cl`/`llvm-lib` for Windows ARM64 assembly deps.
    pub const WINDOWS_ARM64_LLVM: &str = "windows-arm64-llvm";
    /// Java runtime for Gradle.
    pub const JAVA: &str = "java";
    /// `kotlinc` compiler.
    pub const KOTLIN: &str = "kotlin";
    /// `wasm32-unknown-unknown` rustup target.
    pub const WASM32_TARGET: &str = "wasm32-target";
    /// `wasm-pack` binary.
    pub const WASM_PACK: &str = "wasm-pack";
    /// The Espressif `esp` toolchain, its clang/GCC/`rust-src` pieces, and the
    /// `espflash`/`ldproxy` helpers an ESP32 build drives.
    pub const ESP32_TOOLCHAIN: &str = "esp32-toolchain";
    /// Cargo-installed helper binaries the CLI's workflows invoke
    /// (`cargo-nextest` for `water bench`).
    pub const CARGO_HELPERS: &str = "cargo-helpers";
    /// Distribution packages the Linux backends build against.
    pub const LINUX_SYSTEM_PACKAGES: &str = "linux-system-packages";
    /// GTK4/pango pkg-config probes.
    pub const GTK4: &str = "gtk4";
    /// `WinUI` build prerequisites on Windows hosts.
    pub const WINUI: &str = "winui";
    /// `sccache` compile cache.
    pub const SCCACHE: &str = "sccache";
    /// The `[web] package_manager` the current project's `Water.toml` declares.
    pub const WEB_PACKAGE_MANAGER: &str = "web-package-manager";

    /// Every doctor item id in emission order.
    ///
    /// This is the single source of truth for the report's identity set:
    /// [`crate::toolchain::doctor::doctor`], the lib-level ordering test, and
    /// the `water doctor --json` integration test all assert against it.
    pub const ALL: &[&str] = &[
        XCODE,
        IOS_SDK,
        IOS_SIMULATOR_SDK,
        IOS_SIMULATORS,
        MACOS_SDK,
        RUST,
        APPLE_RUST_TARGETS,
        ANDROID_SDK,
        ANDROID_PLATFORM_TOOLS,
        ANDROID_SDK_PLATFORMS,
        ANDROID_BUILD_TOOLS,
        ANDROID_NDK,
        ANDROID_RUST_TARGETS,
        ANDROID_RUN_TARGETS,
        CMAKE,
        WINDOWS_ARM64_LLVM,
        JAVA,
        KOTLIN,
        WASM32_TARGET,
        WASM_PACK,
        ESP32_TOOLCHAIN,
        CARGO_HELPERS,
        LINUX_SYSTEM_PACKAGES,
        GTK4,
        WINUI,
        SCCACHE,
        WEB_PACKAGE_MANAGER,
    ];
}

fn unfixable_message(error: &UnfixableToolchain) -> String {
    format!(
        "Cannot auto-fix: {}. Next step: {}",
        error.message(),
        error.suggestion()
    )
}

/// What `host.cwd()` tells doctor about the surrounding project: the
/// `Water.toml` manifest when the working directory is a project, and the
/// Rust floor the `rust` item enforces — the maximum of the CLI's own
/// `rust-version`, the project's `Cargo.toml` `rust-version`, and the
/// selected framework's.
struct ProjectContext {
    manifest: Option<Manifest>,
    rust_floor: Version,
}

impl ProjectContext {
    /// Whether the project selects a backend — always true for a playground,
    /// whose platform projects the CLI manages on demand.
    fn selects(&self, selected: impl Fn(&crate::backend::Backends) -> bool) -> bool {
        self.manifest.as_ref().is_some_and(|manifest| {
            manifest.package.package_type == PackageType::Playground || selected(&manifest.backends)
        })
    }

    /// The chips the project's ESP32 (Dew) backend can target: the chip
    /// `[backends.esp32]` declares, or every supported chip for a playground.
    /// `None` when no project is present or no ESP32 backend is selected.
    fn esp32_chips(&self) -> Option<eyre::Result<Vec<Esp32Chip>>> {
        let manifest = self.manifest.as_ref()?;
        if let Some(backend) = manifest.backends.esp32() {
            return Some(backend.resolved_chip().map(|chip| vec![chip]));
        }
        (manifest.package.package_type == PackageType::Playground).then(|| {
            Ok(vec![
                Esp32Chip::Esp32S3,
                Esp32Chip::Esp32C3,
                Esp32Chip::Esp32P4,
            ])
        })
    }
}

/// The `rust-version` a `Cargo.toml` root manifest declares, when it parses.
async fn cargo_manifest_rust_version(path: &Path) -> Option<Version> {
    let manifest: toml::Value = toml::from_str(&smol::fs::read_to_string(path).await.ok()?).ok()?;
    manifest_rust_version(&manifest).ok().flatten()
}

async fn project_context(host: &Host) -> ProjectContext {
    let manifest = Manifest::open(host.cwd().join("Water.toml")).await.ok();
    let mut rust_floor = parse_semver_version(CLI_MINIMUM_RUST_VERSION)
        .unwrap_or_else(|_| unreachable!("CARGO_PKG_RUST_VERSION is valid semver"));
    if let Some(manifest) = &manifest {
        // The project's own `rust-version` and the selected framework's both
        // raise the floor; a `waterui_path` checkout's root manifest carries
        // the framework's.
        if let Some(floor) = cargo_manifest_rust_version(&host.cwd().join("Cargo.toml")).await {
            rust_floor = rust_floor.max(floor);
        }
        let framework_floor = match (&manifest.framework, &manifest.waterui_path) {
            (Some(framework), _) => framework.rust_version().cloned(),
            (None, Some(waterui_path)) => {
                let path = Path::new(waterui_path);
                let root = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    host.cwd().join(path)
                };
                cargo_manifest_rust_version(&root.join("Cargo.toml")).await
            }
            (None, None) => None,
        };
        if let Some(floor) = framework_floor {
            rust_floor = rust_floor.max(floor);
        }
    }
    ProjectContext {
        manifest,
        rust_floor,
    }
}

async fn push_toolchain_check<T>(
    host: &Host,
    items: &mut Vec<DoctorItem>,
    id: &'static str,
    name: &'static str,
    fixable_message: &'static str,
    toolchain: T,
) where
    T: Toolchain,
    T::Installation: Send + 'static,
{
    match toolchain.check(host).await {
        Ok(()) => items.push(DoctorItem::ok(id, name)),
        Err(ToolchainError::Fixable(installation)) => {
            items.push(DoctorItem::fixable(
                id,
                name,
                fixable_message,
                installation,
                host,
            ));
        }
        Err(ToolchainError::Unfixable(error)) => {
            items.push(DoctorItem::missing(id, name, unfixable_message(&error)));
        }
    }
}

async fn push_toolchain_check_with_unfixable<T, F>(
    host: &Host,
    items: &mut Vec<DoctorItem>,
    id: &'static str,
    name: &'static str,
    fixable_message: &'static str,
    toolchain: T,
    unfixable_message_fn: F,
) where
    T: Toolchain,
    T::Installation: Send + 'static,
    F: FnOnce(&UnfixableToolchain) -> String,
{
    match toolchain.check(host).await {
        Ok(()) => items.push(DoctorItem::ok(id, name)),
        Err(ToolchainError::Fixable(installation)) => {
            items.push(DoctorItem::fixable(
                id,
                name,
                fixable_message,
                installation,
                host,
            ));
        }
        Err(ToolchainError::Unfixable(error)) => {
            items.push(DoctorItem::missing(id, name, unfixable_message_fn(&error)));
        }
    }
}

async fn push_apple_checks(host: &Host, items: &mut Vec<DoctorItem>) {
    if !cfg!(target_os = "macos") {
        items.push(DoctorItem::skipped(ids::XCODE, "Xcode"));
        items.push(DoctorItem::skipped(ids::IOS_SDK, "iOS SDK"));
        items.push(DoctorItem::skipped(
            ids::IOS_SIMULATOR_SDK,
            "iOS Simulator SDK",
        ));
        items.push(DoctorItem::skipped(ids::IOS_SIMULATORS, "iOS Simulators"));
        items.push(DoctorItem::skipped(ids::MACOS_SDK, "macOS SDK"));
        return;
    }

    push_simple_check(items, ids::XCODE, "Xcode", Xcode.check(host).await);
    push_simple_check(
        items,
        ids::IOS_SDK,
        "iOS SDK",
        AppleSdk::Ios.check(host).await,
    );
    push_simple_check(
        items,
        ids::IOS_SIMULATOR_SDK,
        "iOS Simulator SDK",
        AppleSdk::IosSimulator.check(host).await,
    );
    push_ios_simulator_check(host, items).await;
    push_simple_check(
        items,
        ids::MACOS_SDK,
        "macOS SDK",
        AppleSdk::Macos.check(host).await,
    );
}

fn push_simple_check(
    items: &mut Vec<DoctorItem>,
    id: &'static str,
    name: &'static str,
    result: Result<(), impl std::fmt::Display>,
) {
    match result {
        Ok(()) => items.push(DoctorItem::ok(id, name)),
        Err(error) => items.push(DoctorItem::missing(id, name, error.to_string())),
    }
}

async fn push_ios_simulator_check(host: &Host, items: &mut Vec<DoctorItem>) {
    match AppleSimulator::scan_ios(host).await {
        Ok(simulators) if simulators.is_empty() => items.push(DoctorItem::missing(
            ids::IOS_SIMULATORS,
            "iOS Simulators",
            "No iOS simulators available. Install a simulator runtime in Xcode Settings > Platforms.",
        )),
        Ok(_) => items.push(DoctorItem::ok(ids::IOS_SIMULATORS, "iOS Simulators")),
        Err(error) => items.push(DoctorItem::missing(
            ids::IOS_SIMULATORS,
            "iOS Simulators",
            format!("Failed to list iOS simulators: {error}"),
        )),
    }
}

async fn push_android_sdk_checks(host: &Host, items: &mut Vec<DoctorItem>) -> bool {
    push_toolchain_check(
        host,
        items,
        ids::ANDROID_SDK,
        "Android SDK",
        "Android SDK is missing (automatic install is supported on this host)",
        AndroidSdk,
    )
    .await;

    AndroidSdk::sdkmanager_path(host).await.is_some()
}

async fn push_android_component_checks(
    host: &Host,
    items: &mut Vec<DoctorItem>,
    sdk_ready: bool,
    project: &ProjectContext,
) {
    if sdk_ready {
        push_toolchain_check(
            host,
            items,
            ids::ANDROID_PLATFORM_TOOLS,
            "Android Platform-Tools (adb)",
            "Required for `water run --platform android`",
            AndroidPlatformTools,
        )
        .await;
        push_toolchain_check(
            host,
            items,
            ids::ANDROID_SDK_PLATFORMS,
            "Android SDK Platforms",
            "Required for Android build/package workflows",
            AndroidSdkPlatforms,
        )
        .await;
        push_toolchain_check(
            host,
            items,
            ids::ANDROID_BUILD_TOOLS,
            "Android SDK Build-Tools (d8)",
            "Required for Android build/package workflows",
            AndroidBuildTools,
        )
        .await;
        push_toolchain_check(
            host,
            items,
            ids::ANDROID_NDK,
            "Android NDK",
            "Required for Android build/package workflows",
            AndroidNdk,
        )
        .await;
    } else {
        push_blocked_android_component_checks(items);
    }

    // The rustup targets only need rustup — they are probed regardless of
    // SDK state, and only when the project actually builds for Android.
    if project.selects(|backends| backends.android().is_some()) {
        push_toolchain_check(
            host,
            items,
            ids::ANDROID_RUST_TARGETS,
            "Android Rust Targets",
            "Required for Android Rust cross-compilation",
            AndroidRustTargets::default(),
        )
        .await;
    } else {
        items.push(DoctorItem::skipped_with_message(
            ids::ANDROID_RUST_TARGETS,
            "Android Rust Targets",
            "No Android backend is selected in this project's Water.toml.",
        ));
    }
}

/// Items emitted when the SDK is missing, in the same order as the probed
/// branch above so `--json` ordering does not depend on the diagnosis path.
/// `android-rust-targets` is deliberately absent: it needs only rustup, so it
/// is probed (or skipped) independently of the SDK.
fn push_blocked_android_component_checks(items: &mut Vec<DoctorItem>) {
    for (id, name) in [
        (ids::ANDROID_PLATFORM_TOOLS, "Android Platform-Tools (adb)"),
        (ids::ANDROID_SDK_PLATFORMS, "Android SDK Platforms"),
        (ids::ANDROID_BUILD_TOOLS, "Android SDK Build-Tools (d8)"),
        (ids::ANDROID_NDK, "Android NDK"),
    ] {
        items.push(DoctorItem::missing(
            id,
            name,
            "Blocked: Android SDK / `sdkmanager` is not ready yet. Fix Android SDK first.",
        ));
    }
}

async fn push_android_run_target_check(host: &Host, items: &mut Vec<DoctorItem>) {
    if AndroidSdk::adb_path(host).is_none() {
        items.push(DoctorItem::missing(
            ids::ANDROID_RUN_TARGETS,
            "Android Run Targets",
            "Blocked: Android Platform-Tools (`adb`) is not ready yet.",
        ));
        return;
    }

    match AndroidDevice::scan(host).await {
        Ok(devices) if !devices.is_empty() => {
            items.push(DoctorItem::ok(ids::ANDROID_RUN_TARGETS, "Android Run Targets"));
        }
        Ok(_) => match AndroidPlatform::list_avds(host).await {
            Ok(avds) if !avds.is_empty() => {
                items.push(DoctorItem::ok(ids::ANDROID_RUN_TARGETS, "Android Run Targets"));
            }
            Ok(_) => items.push(DoctorItem::missing(
                ids::ANDROID_RUN_TARGETS,
                "Android Run Targets",
                "No connected Android devices and no emulator AVDs were found. Connect a device or create an AVD.",
            )),
            Err(error) => items.push(DoctorItem::missing(
                ids::ANDROID_RUN_TARGETS,
                "Android Run Targets",
                format!(
                    "No connected Android devices and failed to list AVDs: {error}. Install Android emulator components or connect a device."
                ),
            )),
        },
        Err(error) => items.push(DoctorItem::missing(
            ids::ANDROID_RUN_TARGETS,
            "Android Run Targets",
            format!("Failed to query Android devices via adb: {error}"),
        )),
    }
}

async fn push_desktop_and_web_checks(
    host: &Host,
    items: &mut Vec<DoctorItem>,
    project: &ProjectContext,
) {
    push_toolchain_check(
        host,
        items,
        ids::CMAKE,
        "Host CMake",
        "Required for native Rust dependencies in Android builds",
        Cmake::default(),
    )
    .await;

    if WindowsArm64LlvmToolchain::required_on_host() {
        push_toolchain_check(
            host,
            items,
            ids::WINDOWS_ARM64_LLVM,
            "Windows ARM64 LLVM toolchain",
            "Required by native assembly dependencies in Windows ARM64 hydrolysis builds",
            WindowsArm64LlvmToolchain,
        )
        .await;
    } else {
        items.push(DoctorItem::skipped_with_message(
            ids::WINDOWS_ARM64_LLVM,
            "Windows ARM64 LLVM toolchain",
            "Only required on Windows ARM64 hosts for native assembly dependencies.",
        ));
    }

    push_toolchain_check(
        host,
        items,
        ids::JAVA,
        "Java",
        "Required for Android Gradle builds",
        Java,
    )
    .await;
    push_toolchain_check(
        host,
        items,
        ids::KOTLIN,
        "Kotlin",
        "Required for Android Kotlin helper compilation",
        Kotlin,
    )
    .await;

    if project.selects(|backends| backends.hydrolysis().is_some()) {
        push_toolchain_check_with_unfixable(
            host,
            items,
            ids::WASM32_TARGET,
            "Rust wasm32 target",
            "wasm32-unknown-unknown target not installed",
            wasm32_target(),
            ToString::to_string,
        )
        .await;
        push_toolchain_check_with_unfixable(
            host,
            items,
            ids::WASM_PACK,
            "wasm-pack",
            "wasm-pack not found (required for web packaging)",
            WasmPack,
            ToString::to_string,
        )
        .await;
    } else {
        items.push(DoctorItem::skipped_with_message(
            ids::WASM32_TARGET,
            "Rust wasm32 target",
            "No hydrolysis (web) backend is selected in this project's Water.toml.",
        ));
        items.push(DoctorItem::skipped_with_message(
            ids::WASM_PACK,
            "wasm-pack",
            "No hydrolysis (web) backend is selected in this project's Water.toml.",
        ));
    }
}

/// The Espressif-side toolchain — `esp` Rust fork, clang/GCC, `rust-src`,
/// `espflash`/`ldproxy`, QEMU — when the project selects a Dew/ESP32 backend.
async fn push_esp32_check(host: &Host, items: &mut Vec<DoctorItem>, project: &ProjectContext) {
    const NAME: &str = "ESP32 toolchain";
    let Some(chips) = project.esp32_chips() else {
        items.push(DoctorItem::skipped_with_message(
            ids::ESP32_TOOLCHAIN,
            NAME,
            "No ESP32 backend is selected in this project's Water.toml.",
        ));
        return;
    };
    let chips = match chips {
        Ok(chips) => chips,
        Err(error) => {
            items.push(DoctorItem::missing(
                ids::ESP32_TOOLCHAIN,
                NAME,
                format!("Invalid `[backends.esp32]` configuration: {error}"),
            ));
            return;
        }
    };
    match Esp32Toolchain::new(chips).check(host).await {
        Ok(()) => items.push(DoctorItem::ok(ids::ESP32_TOOLCHAIN, NAME)),
        Err(ToolchainError::Fixable(installation)) => items.push(DoctorItem::fixable(
            ids::ESP32_TOOLCHAIN,
            NAME,
            installation.describe(),
            installation,
            host,
        )),
        Err(ToolchainError::Unfixable(error)) => items.push(DoctorItem::missing(
            ids::ESP32_TOOLCHAIN,
            NAME,
            unfixable_message(&error),
        )),
    }
}

/// The cargo-installed helper binaries a project's workflows invoke —
/// `cargo-nextest` for `water bench`. Platform helpers that are also cargo
/// installs (`wasm-pack`, `espflash`/`ldproxy`) are covered by their own
/// platform items.
async fn push_cargo_helpers_check(host: &Host, items: &mut Vec<DoctorItem>) {
    const NAME: &str = "Cargo helpers";
    match CargoHelpers::new(["cargo-nextest"]).check(host).await {
        Ok(()) => items.push(DoctorItem::ok(ids::CARGO_HELPERS, NAME)),
        Err(ToolchainError::Fixable(installation)) => items.push(DoctorItem::fixable(
            ids::CARGO_HELPERS,
            NAME,
            installation.describe(),
            installation,
            host,
        )),
        Err(ToolchainError::Unfixable(error)) => items.push(DoctorItem::missing(
            ids::CARGO_HELPERS,
            NAME,
            unfixable_message(&error),
        )),
    }
}

async fn push_linux_checks(host: &Host, items: &mut Vec<DoctorItem>) {
    if !cfg!(target_os = "linux") {
        items.push(DoctorItem::skipped(
            ids::LINUX_SYSTEM_PACKAGES,
            "Linux system packages",
        ));
        items.push(DoctorItem::skipped(ids::GTK4, "GTK4"));
        return;
    }

    let linux_packages_fixable = match LinuxSystemToolchain.check(host).await {
        Ok(()) => {
            items.push(DoctorItem::ok(
                ids::LINUX_SYSTEM_PACKAGES,
                "Linux system packages",
            ));
            false
        }
        Err(ToolchainError::Fixable(installation)) => {
            let msg = format!(
                "Missing packages for {}: {}. Install command: {}",
                installation.package_manager_name(),
                installation.missing_packages().join(", "),
                installation.install_command_hint(),
            );
            items.push(DoctorItem::fixable(
                ids::LINUX_SYSTEM_PACKAGES,
                "Linux system packages",
                msg,
                installation,
                host,
            ));
            true
        }
        Err(ToolchainError::Unfixable(error)) => {
            items.push(DoctorItem::missing(
                ids::LINUX_SYSTEM_PACKAGES,
                "Linux system packages",
                unfixable_message(&error),
            ));
            false
        }
    };

    match Gtk4Toolchain.check(host).await {
        Ok(()) => items.push(DoctorItem::ok(ids::GTK4, "GTK4")),
        Err(ToolchainError::Fixable(installation)) => {
            items.push(DoctorItem::fixable(
                ids::GTK4,
                "GTK4",
                "GTK4 dependencies are missing",
                installation,
                host,
            ));
        }
        Err(ToolchainError::Unfixable(error)) => {
            if linux_packages_fixable {
                items.push(DoctorItem::missing(
                    ids::GTK4,
                    "GTK4",
                    "GTK4 probe failed because required Linux packages are missing. Run `water doctor --fix` to install Linux system packages, then re-run `water doctor`.",
                ));
            } else {
                items.push(DoctorItem::missing(
                    ids::GTK4,
                    "GTK4",
                    unfixable_message(&error),
                ));
            }
        }
    }
}

async fn push_windows_checks(host: &Host, items: &mut Vec<DoctorItem>) {
    if !cfg!(target_os = "windows") {
        items.push(DoctorItem::skipped(ids::WINUI, "WinUI"));
        return;
    }

    push_toolchain_check(
        host,
        items,
        ids::WINUI,
        "WinUI",
        "WinUI build prerequisites are missing",
        WinUiToolchain,
    )
    .await;
}

/// Run diagnostics on all toolchains on `host` and return a report.
///
/// Item order is fixed and platform branching is driven by `cfg!` plus the
/// project context `host.cwd()` resolves, so two runs on equal hosts in equal
/// projects produce identical item sequences — the property the
/// orchestration tests and `--json` consumers rely on. Without a `Water.toml`
/// the host-level checks still run and every project-gated item reports
/// `skipped`.
pub async fn doctor(host: &Host) -> Vec<DoctorItem> {
    let project = project_context(host).await;
    let mut items = Vec::new();
    push_apple_checks(host, &mut items).await;
    push_rust_toolchain_check(host, &mut items, &project).await;
    push_apple_rust_targets(host, &mut items, &project).await;
    let sdk_ready = push_android_sdk_checks(host, &mut items).await;
    push_android_component_checks(host, &mut items, sdk_ready, &project).await;
    push_android_run_target_check(host, &mut items).await;
    push_desktop_and_web_checks(host, &mut items, &project).await;
    push_esp32_check(host, &mut items, &project).await;
    push_cargo_helpers_check(host, &mut items).await;
    push_linux_checks(host, &mut items).await;
    push_windows_checks(host, &mut items).await;
    push_toolchain_check(
        host,
        &mut items,
        ids::SCCACHE,
        "sccache",
        "sccache not found (recommended for faster builds)",
        Sccache,
    )
    .await;
    push_web_package_manager_check(host, &mut items, &project).await;

    items
}

/// The iOS device and simulator rustup targets an Apple-backend project
/// needs on its selected toolchain. (The macOS target is the host triple the
/// `rust` item already requires.)
async fn push_apple_rust_targets(
    host: &Host,
    items: &mut Vec<DoctorItem>,
    project: &ProjectContext,
) {
    const NAME: &str = "Apple Rust targets";
    if !cfg!(target_os = "macos") {
        items.push(DoctorItem::skipped_with_message(
            ids::APPLE_RUST_TARGETS,
            NAME,
            "Apple platforms can only be built on macOS.",
        ));
        return;
    }
    if !project.selects(|backends| backends.apple().is_some()) {
        items.push(DoctorItem::skipped_with_message(
            ids::APPLE_RUST_TARGETS,
            NAME,
            "No Apple backend is selected in this project's Water.toml.",
        ));
        return;
    }
    push_toolchain_check(
        host,
        items,
        ids::APPLE_RUST_TARGETS,
        NAME,
        "Required iOS targets are missing on the selected Rust toolchain",
        crate::toolchain::rust::SelectedToolchainTargets::new(vec![
            TargetPlatform::IOS.triple().to_string(),
            TargetPlatform::IOSSimulator.triple().to_string(),
        ]),
    )
    .await;
}

/// Checks the `[web] package_manager` the project's `Water.toml` declares.
/// Only the declared manager is probed — a project on `pnpm` is never
/// reported healthy because `bun` happens to be installed.
async fn push_web_package_manager_check(
    host: &Host,
    items: &mut Vec<DoctorItem>,
    project: &ProjectContext,
) {
    let Some(web) = project
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.web.as_ref())
    else {
        return;
    };
    let package_manager = web.package_manager;
    let name: &'static str = match package_manager {
        crate::web::PackageManager::Bun => "bun (web package manager)",
        crate::web::PackageManager::Pnpm => "pnpm (web package manager)",
        crate::web::PackageManager::Npm => "npm (web package manager)",
        crate::web::PackageManager::Yarn => "yarn (web package manager)",
    };
    push_toolchain_check(
        host,
        items,
        ids::WEB_PACKAGE_MANAGER,
        name,
        package_manager.install_hint(),
        PackageManagerToolchain(package_manager),
    )
    .await;
}

async fn push_rust_toolchain_check(
    host: &Host,
    items: &mut Vec<DoctorItem>,
    project: &ProjectContext,
) {
    match RustToolchain::new(&project.rust_floor).check(host).await {
        Ok(()) => items.push(DoctorItem::ok(ids::RUST, "Rust toolchain")),
        Err(ToolchainError::Fixable(installation)) => {
            items.push(DoctorItem::fixable(
                ids::RUST,
                "Rust toolchain",
                format!(
                    "Rust toolchain is missing, outdated, or incomplete. Planned automatic fixes: {}",
                    installation.summary()
                ),
                installation,
                host,
            ));
        }
        Err(ToolchainError::Unfixable(error)) => items.push(DoctorItem::missing(
            ids::RUST,
            "Rust toolchain",
            unfixable_message(&error),
        )),
    }
}
#[cfg(test)]
mod tests {
    use super::{CheckStatus, doctor, ids};
    use crate::toolchain::testing::TestMachine;

    const ANDROID_COMPONENT_IDS: &[&str] = &[
        ids::ANDROID_PLATFORM_TOOLS,
        ids::ANDROID_SDK_PLATFORMS,
        ids::ANDROID_BUILD_TOOLS,
        ids::ANDROID_NDK,
    ];

    /// A minimal `Water.toml` app manifest; `extra` is appended verbatim
    /// (`[backends.*]`, `[web]`, ...).
    fn manifest(extra: &str) -> String {
        format!(
            "[package]\ntype = \"app\"\nname = \"Fixture\"\nbundle_identifier = \"dev.waterui.fixture\"\n\n{extra}"
        )
    }

    fn ids_of(items: &[super::DoctorItem]) -> Vec<&'static str> {
        items.iter().map(|item| item.id).collect()
    }

    fn item<'a>(items: &'a [super::DoctorItem], id: &str) -> &'a super::DoctorItem {
        items
            .iter()
            .find(|item| item.id == id)
            .unwrap_or_else(|| panic!("doctor report must contain `{id}`"))
    }

    #[test]
    fn doctor_emits_every_item_in_stable_order() {
        let machine = TestMachine::new();
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));
        // `WEB_PACKAGE_MANAGER` only emits when the current directory's
        // `Water.toml` declares a `[web]` section; the test CWD has none.
        let expected: Vec<&'static str> = ids::ALL
            .iter()
            .copied()
            .filter(|id| *id != ids::WEB_PACKAGE_MANAGER)
            .collect();
        assert_eq!(ids_of(&items), expected);
    }

    #[test]
    fn doctor_blocks_android_components_when_sdk_absent() {
        let machine = TestMachine::new();
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));

        assert_eq!(item(&items, ids::ANDROID_SDK).status, CheckStatus::Missing);
        for id in ANDROID_COMPONENT_IDS {
            let component = item(&items, id);
            assert_eq!(component.status, CheckStatus::Missing, "{id}");
            assert!(
                component
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains("Blocked")),
                "{id} must carry the blocked diagnostic: {:?}",
                component.message
            );
            assert!(
                !component.is_fixable(),
                "blocked {id} must not offer an install"
            );
        }

        // Without a manifest the Android rust targets are not required, so
        // the item is skipped rather than blocked or probed.
        assert_eq!(
            item(&items, ids::ANDROID_RUST_TARGETS).status,
            CheckStatus::Skipped
        );

        let run_targets = item(&items, ids::ANDROID_RUN_TARGETS);
        assert_eq!(run_targets.status, CheckStatus::Missing);
        assert!(
            run_targets
                .message
                .as_deref()
                .is_some_and(|message| message.contains("Blocked"))
        );
    }

    #[test]
    fn doctor_probes_android_components_when_sdk_ready() {
        let machine = TestMachine::new();
        machine.file("Water.toml", &manifest("[backends.android]\n"));
        let sdk = machine.install_android_sdk();
        let host = machine.host([(
            String::from("ANDROID_SDK_ROOT"),
            sdk.as_os_str().to_os_string(),
        )]);
        let items = smol::block_on(doctor(&host));

        assert_eq!(item(&items, ids::ANDROID_SDK).status, CheckStatus::Ok);
        for id in ANDROID_COMPONENT_IDS {
            let component = item(&items, id);
            assert_eq!(component.status, CheckStatus::Missing, "{id}");
            assert!(
                !component
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains("Blocked")),
                "{id} must be a real diagnosis, not the blocked marker: {:?}",
                component.message
            );
        }

        // adb / platforms / build-tools / NDK are installable via sdkmanager.
        for id in [
            ids::ANDROID_PLATFORM_TOOLS,
            ids::ANDROID_SDK_PLATFORMS,
            ids::ANDROID_BUILD_TOOLS,
            ids::ANDROID_NDK,
        ] {
            assert!(item(&items, id).is_fixable(), "{id} must be fixable");
        }
        // The manifest selects the Android backend, so the rustup targets are
        // probed; with no rustup on the fake PATH they are unfixable.
        let rust_targets = item(&items, ids::ANDROID_RUST_TARGETS);
        assert_eq!(rust_targets.status, CheckStatus::Missing);
        assert!(!rust_targets.is_fixable());
    }

    #[test]
    fn doctor_apple_items_match_platform() {
        let machine = TestMachine::new();
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));
        for id in [
            ids::XCODE,
            ids::IOS_SDK,
            ids::IOS_SIMULATOR_SDK,
            ids::IOS_SIMULATORS,
            ids::MACOS_SDK,
        ] {
            let status = item(&items, id).status;
            if cfg!(target_os = "macos") {
                assert_eq!(
                    status,
                    CheckStatus::Missing,
                    "{id} is probed on macOS and missing on a bare host"
                );
            } else {
                assert_eq!(
                    status,
                    CheckStatus::Skipped,
                    "{id} must be skipped off macOS"
                );
            }
        }
    }

    /// The staged `simctl list devices --json` transcript reports one healthy
    /// iPhone, so `ios-simulators` comes back `Ok` — the fake `xcrun` must
    /// answer the query and the transcript's `dataPath` must exist.
    #[test]
    #[cfg(target_os = "macos")]
    fn doctor_ios_simulators_ok_when_simctl_reports_healthy_device() {
        let machine = TestMachine::new();
        machine.install("xcrun");
        // Retarget the transcript's `/fake/...` paths into the scratch root
        // so `data_path.exists()` holds on the declared host.
        machine.dir(
            "Library/Developer/CoreSimulator/Devices/3E8B0C4F-0000-4000-8000-000000000001/data",
        );
        let transcript = include_str!("testdata/simctl_devices.json")
            .replace("/fake/", &format!("{}/", machine.root().display()));
        machine.respond("XCRUN_SIMCTL_DEVICES", &transcript);
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));
        assert_eq!(
            item(&items, ids::IOS_SIMULATORS).status,
            CheckStatus::Ok,
            "a healthy simctl device must satisfy ios-simulators"
        );
    }

    #[test]
    fn doctor_linux_items_match_platform() {
        let machine = TestMachine::new();
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));
        for id in [ids::LINUX_SYSTEM_PACKAGES, ids::GTK4] {
            let status = item(&items, id).status;
            if cfg!(target_os = "linux") {
                assert_eq!(
                    status,
                    CheckStatus::Missing,
                    "{id} is probed on Linux and missing on a bare host"
                );
            } else {
                assert_eq!(
                    status,
                    CheckStatus::Skipped,
                    "{id} must be skipped off Linux"
                );
            }
        }
    }

    #[test]
    fn doctor_windows_llvm_skipped_where_not_required() {
        let machine = TestMachine::new();
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));
        let status = item(&items, ids::WINDOWS_ARM64_LLVM).status;
        if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
            assert_eq!(status, CheckStatus::Missing);
        } else {
            assert_eq!(status, CheckStatus::Skipped);
        }
    }

    #[test]
    fn doctor_fixable_and_manual_classification() {
        let machine = TestMachine::new();
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));

        // No rust tools at all → manual fix required.
        let rust = item(&items, ids::RUST);
        assert_eq!(rust.status, CheckStatus::Missing);
        assert!(!rust.is_fixable());

        // The cargo helpers need `cargo` to install → manual without it.
        let cargo_helpers = item(&items, ids::CARGO_HELPERS);
        assert_eq!(cargo_helpers.status, CheckStatus::Missing);
        assert!(!cargo_helpers.is_fixable());

        // On Linux a bare host still plans an SDK install into ~/Android/Sdk.
        #[cfg(target_os = "linux")]
        assert!(item(&items, ids::ANDROID_SDK).is_fixable());
    }

    /// With a hydrolysis backend selected and `cargo` on PATH, a missing
    /// `wasm-pack` is a `cargo install` away → fixable.
    #[test]
    fn doctor_wasm_pack_fixable_when_hydrolysis_selected() {
        let machine = TestMachine::new();
        machine.file("Water.toml", &manifest("[backends.hydrolysis]\n"));
        machine.install("cargo");
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));

        let wasm_pack = item(&items, ids::WASM_PACK);
        assert_eq!(wasm_pack.status, CheckStatus::Missing);
        assert!(wasm_pack.is_fixable());
    }

    /// Project-gated items must not report failures for projects that do not
    /// select the platform.
    #[test]
    fn doctor_skips_platform_items_no_project_selects() {
        let machine = TestMachine::new();
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));

        for id in [
            ids::ANDROID_RUST_TARGETS,
            ids::WASM32_TARGET,
            ids::WASM_PACK,
            ids::ESP32_TOOLCHAIN,
            ids::APPLE_RUST_TARGETS,
        ] {
            assert_eq!(
                item(&items, id).status,
                CheckStatus::Skipped,
                "{id} must be skipped on a project-less host"
            );
        }
    }

    /// Every platform the manifest selects is probed, even on a bare host.
    #[test]
    fn doctor_probes_the_backends_a_manifest_selects() {
        let machine = TestMachine::new();
        machine.file(
            "Water.toml",
            &manifest(
                "[backends.android]\n\n[backends.hydrolysis]\n\n[backends.esp32]\nchip = \"esp32c3\"\n\n[backends.apple]\nscheme = \"Fixture\"\n",
            ),
        );
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));

        for id in [
            ids::ANDROID_RUST_TARGETS,
            ids::WASM32_TARGET,
            ids::WASM_PACK,
            ids::ESP32_TOOLCHAIN,
        ] {
            assert_eq!(
                item(&items, id).status,
                CheckStatus::Missing,
                "selected {id} must be probed on a bare host"
            );
        }
        if cfg!(target_os = "macos") {
            assert_eq!(
                item(&items, ids::APPLE_RUST_TARGETS).status,
                CheckStatus::Missing
            );
        }
    }

    /// An `[backends.esp32]` chip the CLI does not support is a diagnostic,
    /// not a skipped item.
    #[test]
    fn doctor_reports_invalid_esp32_chip() {
        let machine = TestMachine::new();
        machine.file(
            "Water.toml",
            &manifest("[backends.esp32]\nchip = \"atmega328p\"\n"),
        );
        let host = machine.host(Vec::<(String, String)>::new());
        let items = smol::block_on(doctor(&host));

        let esp32 = item(&items, ids::ESP32_TOOLCHAIN);
        assert_eq!(esp32.status, CheckStatus::Missing);
        assert!(
            esp32
                .message
                .as_deref()
                .is_some_and(|message| message.contains("Invalid")),
            "the invalid chip must be diagnosed: {:?}",
            esp32.message
        );
    }

    /// A `--fix` pass runs each fixable item's install; a re-diagnosis must
    /// then observe the repair — the fix-loop property `water doctor --fix`
    /// relies on.
    #[test]
    #[cfg(unix)]
    fn doctor_fix_loop_repairs_pinned_toolchain() {
        let machine = TestMachine::new();
        machine.file("Water.toml", &manifest(""));
        machine.file("rust-toolchain.toml", "[toolchain]\nchannel = \"1.90\"\n");
        for tool in ["rustup", "cargo", "rustc"] {
            machine.install(tool);
        }
        let host = machine.host([
            (
                String::from("WATERUI_FAKE_RUSTUP_TOOLCHAIN_NOT_INSTALLED"),
                String::from("1.90"),
            ),
            (
                String::from("WATERUI_FAKE_RUSTC_VERSION"),
                String::from("99.0.0"),
            ),
            (
                String::from("WATERUI_FAKE_RUSTC_HOST"),
                String::from("x86_64-unknown-fake"),
            ),
            (
                String::from("WATERUI_FAKE_RUSTUP_INSTALLED_TARGETS"),
                String::from("x86_64-unknown-fake"),
            ),
        ]);

        let items = smol::block_on(doctor(&host));
        let rust = items
            .into_iter()
            .find(|item| item.id == ids::RUST)
            .expect("rust item");
        assert_eq!(rust.status, CheckStatus::Missing);
        let install = rust.install_fn.expect("the pin repair must be fixable");
        smol::block_on(install()).expect("install must succeed on the fake host");

        let items = smol::block_on(doctor(&host));
        assert_eq!(
            item(&items, ids::RUST).status,
            CheckStatus::Ok,
            "after `rustup toolchain install 1.90` the rust item must be ok"
        );
    }

    #[test]
    #[cfg(unix)]
    fn doctor_reports_complete_android_chain_when_fully_staged() {
        let machine = TestMachine::new();
        let sdk = machine.install_android_sdk();
        machine.install_adb();
        machine.install_android_platform("android-37.0");
        machine.install_android_build_tools("37.0.0");
        machine.install_android_ndk("29.0.14206865");
        machine.install_android_emulator();
        machine.install("rustup");
        machine.file("Water.toml", &manifest("[backends.android]\n"));
        machine.respond("EMULATOR_AVDS", "Medium_Phone_API_37\n");
        machine.respond(
            "RUSTUP_ACTIVE_TOOLCHAIN",
            "stable-x86_64-unknown-fake (default)",
        );
        machine.respond(
            "RUSTUP_INSTALLED_TARGETS",
            &[
                "aarch64-linux-android",
                "armv7-linux-androideabi",
                "i686-linux-android",
                "x86_64-linux-android",
            ]
            .join("\n"),
        );
        let host = machine.host([(
            String::from("ANDROID_SDK_ROOT"),
            sdk.as_os_str().to_os_string(),
        )]);
        let items = smol::block_on(doctor(&host));
        for id in [
            ids::ANDROID_SDK,
            ids::ANDROID_PLATFORM_TOOLS,
            ids::ANDROID_SDK_PLATFORMS,
            ids::ANDROID_BUILD_TOOLS,
            ids::ANDROID_NDK,
            ids::ANDROID_RUST_TARGETS,
            ids::ANDROID_RUN_TARGETS,
        ] {
            assert_eq!(
                item(&items, id).status,
                CheckStatus::Ok,
                "{id} must be ok on a fully staged SDK: {:?}",
                item(&items, id).message
            );
        }
    }
}