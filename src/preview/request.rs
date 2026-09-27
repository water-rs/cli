//! Shared preview request resolution for `water preview` and the MCP
//! `preview` tool.
//!
//! Both entry points accept the same arguments — a `#[preview]` function path
//! or expression target, a frame size, and optional platform/backend/theme
//! overrides — and resolve them through the functions here, so the two can
//! never drift apart.

use clap::ValueEnum;
use eyre::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::apple::toolchain::AppleSdk;
use crate::platform::TargetPlatform;
use crate::preview::protocol::{AppError, DylibId, function_path_to_symbol};
use crate::preview::{
    HydrolysisPreviewSource, HydrolysisPreviewTheme, PreviewPlatform, PreviewSession,
};
use crate::toolchain_checks;

/// Default frame size shared by `water preview --frame` and the MCP `preview`
/// tool's `frame` argument.
pub const DEFAULT_FRAME: &str = "375x667";

/// Target platform for preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CliPreviewPlatform {
    /// iOS Simulator.
    Ios,
    /// macOS.
    Macos,
    /// Android Emulator.
    Android,
    /// Linux (Hydrolysis).
    Linux,
    /// Windows (Hydrolysis).
    Windows,
}

impl CliPreviewPlatform {
    /// The support-app [`PreviewPlatform`] this platform renders through.
    ///
    /// Hydrolysis platforms render in-process through the managed backend
    /// binary — they have no support app, so they return `None`.
    #[must_use]
    pub const fn support_app_platform(self) -> Option<PreviewPlatform> {
        match self {
            Self::Ios => Some(PreviewPlatform::IosSimulator),
            Self::Macos => Some(PreviewPlatform::Macos),
            Self::Android => Some(PreviewPlatform::Android),
            Self::Linux | Self::Windows => None,
        }
    }

    /// The build target a Hydrolysis preview on this platform compiles for.
    ///
    /// `resolve_preview_backend` admits Hydrolysis only on the desktop
    /// platforms, so a resolved Hydrolysis request never hits the `None` arm.
    #[must_use]
    pub const fn hydrolysis_target_platform(self) -> Option<TargetPlatform> {
        match self {
            Self::Macos => Some(TargetPlatform::MacOS),
            Self::Linux => Some(TargetPlatform::Linux),
            Self::Windows => Some(TargetPlatform::Windows),
            Self::Ios | Self::Android => None,
        }
    }
}

/// Rendering backend for preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CliPreviewBackend {
    /// Apple preview support app.
    Apple,
    /// Android preview support app.
    Android,
    /// Hydrolysis direct renderer.
    Hydrolysis,
}

/// Theme package for Hydrolysis preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CliHydrolysisPreviewTheme {
    /// Material Design 3 theme package.
    Material3,
}

impl From<CliHydrolysisPreviewTheme> for HydrolysisPreviewTheme {
    fn from(value: CliHydrolysisPreviewTheme) -> Self {
        match value {
            CliHydrolysisPreviewTheme::Material3 => Self::Material3,
        }
    }
}

/// What a preview render draws: a `#[preview]` function exported from the
/// project crate, or an inline `WaterUI` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewTarget {
    /// A `#[preview]` function: its crate-relative path and export symbol.
    Function {
        /// Function path as written, e.g. `views::home`.
        function_path: String,
        /// Export symbol the preview machinery looks up.
        symbol: String,
    },
    /// An inline `WaterUI` expression returning `impl View`.
    Expression {
        /// The expression source, e.g. `text("hello")`.
        expression: String,
    },
}

impl PreviewTarget {
    /// Human-readable target name for logs and output file names.
    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            Self::Function { symbol, .. } => symbol,
            Self::Expression { expression } => expression,
        }
    }

    /// The [`HydrolysisPreviewSource`] for this target.
    #[must_use]
    pub fn hydrolysis_source(&self) -> HydrolysisPreviewSource<'_> {
        match self {
            Self::Function { symbol, .. } => HydrolysisPreviewSource::Symbol(symbol),
            Self::Expression { expression } => HydrolysisPreviewSource::Expression(expression),
        }
    }
}

/// A fully resolved preview render, ready to hand to the Hydrolysis or
/// support-app execution path.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRequest {
    /// Resolved target platform.
    pub platform: CliPreviewPlatform,
    /// Resolved rendering backend.
    pub backend: CliPreviewBackend,
    /// Hydrolysis theme package — `Some` iff `backend` is
    /// [`CliPreviewBackend::Hydrolysis`].
    pub hydrolysis_theme: Option<HydrolysisPreviewTheme>,
    /// What to render.
    pub target: PreviewTarget,
    /// Frame width in logical units.
    pub width: f32,
    /// Frame height in logical units.
    pub height: f32,
}

/// Parse frame size from a `WIDTHxHEIGHT` string.
///
/// # Errors
/// Returns an error if the format is wrong or a dimension is not a positive
/// finite number.
pub fn parse_frame(s: &str) -> Result<(f32, f32)> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() != 2 {
        bail!("Invalid frame format: expected WIDTHxHEIGHT (e.g., 375x667)");
    }

    let width: f32 = parts[0]
        .parse()
        .map_err(|_| eyre::eyre!("Invalid frame width"))?;
    let height: f32 = parts[1]
        .parse()
        .map_err(|_| eyre::eyre!("Invalid frame height"))?;

    if !width.is_finite() || width <= 0.0 {
        bail!("Invalid frame width: must be a positive finite number");
    }
    if !height.is_finite() || height <= 0.0 {
        bail!("Invalid frame height: must be a positive finite number");
    }

    Ok((width, height))
}

/// Resolve the preview target: `expr` forces expression mode, and a target
/// that is not a Rust path is treated as an expression either way.
#[must_use]
pub fn resolve_preview_target(
    crate_name: &str,
    target: &str,
    force_expression: bool,
) -> PreviewTarget {
    if force_expression || !is_function_path(target) {
        return PreviewTarget::Expression {
            expression: target.to_string(),
        };
    }

    PreviewTarget::Function {
        function_path: target.to_string(),
        symbol: function_path_to_symbol(crate_name, target),
    }
}

fn is_function_path(target: &str) -> bool {
    let mut segments = target.split("::").peekable();
    if segments.peek().is_none() {
        return false;
    }

    segments.all(is_rust_ident)
}

fn is_rust_ident(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Resolve the rendering backend for a platform, applying the override when
/// given.
///
/// # Errors
/// Returns an error if the backend does not support the platform.
pub fn resolve_preview_backend(
    platform: CliPreviewPlatform,
    backend_override: Option<CliPreviewBackend>,
) -> Result<CliPreviewBackend> {
    let default_backend = match platform {
        CliPreviewPlatform::Ios | CliPreviewPlatform::Macos => CliPreviewBackend::Apple,
        CliPreviewPlatform::Android => CliPreviewBackend::Android,
        CliPreviewPlatform::Linux | CliPreviewPlatform::Windows => CliPreviewBackend::Hydrolysis,
    };

    let backend = backend_override.unwrap_or(default_backend);
    let supported = matches!(
        (platform, backend),
        (
            CliPreviewPlatform::Ios | CliPreviewPlatform::Macos,
            CliPreviewBackend::Apple
        ) | (
            CliPreviewPlatform::Macos | CliPreviewPlatform::Linux | CliPreviewPlatform::Windows,
            CliPreviewBackend::Hydrolysis
        ) | (CliPreviewPlatform::Android, CliPreviewBackend::Android)
    );
    if !supported {
        bail!(
            "Preview backend {:?} does not support platform {:?}. Valid combinations: ios/apple, macos/apple, macos/hydrolysis, linux/hydrolysis, windows/hydrolysis, android/android",
            backend,
            platform
        );
    }
    Ok(backend)
}

/// Resolve the preview platform, defaulting to this host's native preview
/// platform.
///
/// # Errors
/// Returns an error on hosts with no native preview platform when no override
/// is given.
pub fn resolve_preview_platform(
    platform_override: Option<CliPreviewPlatform>,
) -> Result<CliPreviewPlatform> {
    if let Some(platform) = platform_override {
        return Ok(platform);
    }
    native_preview_platform()
}

fn native_preview_platform() -> Result<CliPreviewPlatform> {
    native_preview_platform_for_os(std::env::consts::OS).ok_or_else(|| {
        eyre::eyre!(
            "No native preview platform is configured for this host. Pass `--platform` explicitly."
        )
    })
}

/// The preview platform a host OS renders natively: `macos` through the Apple
/// support app, `linux` and `windows` through the Hydrolysis backend — the
/// same renderer `water run` uses on those hosts.
fn native_preview_platform_for_os(os: &str) -> Option<CliPreviewPlatform> {
    match os {
        "macos" => Some(CliPreviewPlatform::Macos),
        "linux" => Some(CliPreviewPlatform::Linux),
        "windows" => Some(CliPreviewPlatform::Windows),
        _ => None,
    }
}

/// `water preview test` runs through Hydrolysis, which renders on the
/// desktop platforms only.
///
/// # Errors
/// Returns an error for any other platform.
pub fn ensure_hydrolysis_preview_platform(platform: CliPreviewPlatform) -> Result<()> {
    if platform.hydrolysis_target_platform().is_none() {
        bail!("`water preview test` supports Hydrolysis on macos, linux and windows only.");
    }
    Ok(())
}

/// Resolve the Hydrolysis theme: defaulted for the Hydrolysis backend,
/// rejected for the others.
///
/// Hydrolysis is the native preview platform on Linux and Windows, so its
/// theme cannot be a required flag there — `material3` is the only theme
/// package today and is the default until a second one exists.
///
/// # Errors
/// Returns an error if the theme is set for a non-Hydrolysis backend.
pub fn resolve_hydrolysis_preview_theme(
    backend: CliPreviewBackend,
    theme: Option<CliHydrolysisPreviewTheme>,
) -> Result<Option<HydrolysisPreviewTheme>> {
    match (backend, theme) {
        (CliPreviewBackend::Hydrolysis, theme) => Ok(Some(
            theme.unwrap_or(CliHydrolysisPreviewTheme::Material3).into(),
        )),
        (_, Some(_)) => {
            bail!("`--theme` is only supported with `--backend hydrolysis`.");
        }
        (_, None) => Ok(None),
    }
}

/// Check the host toolchain required by the resolved backend.
///
/// # Errors
/// Returns an error if a required toolchain component is missing.
pub async fn check_toolchain_for_backend(
    platform: CliPreviewPlatform,
    backend: CliPreviewBackend,
) -> Result<()> {
    let host = crate::toolchain::Host::current();
    match backend {
        CliPreviewBackend::Apple => {
            let sdk = match platform {
                CliPreviewPlatform::Ios => AppleSdk::IosSimulator,
                CliPreviewPlatform::Macos => AppleSdk::Macos,
                CliPreviewPlatform::Android
                | CliPreviewPlatform::Linux
                | CliPreviewPlatform::Windows => {
                    bail!("Internal error: Apple preview backend is not supported on {platform:?}");
                }
            };
            toolchain_checks::check_apple(&host, sdk).await?;
        }
        CliPreviewBackend::Android => {
            if platform != CliPreviewPlatform::Android {
                bail!("Internal error: Android preview backend is not supported on {platform:?}");
            }
            toolchain_checks::check_android_run(&host).await?;
        }
        CliPreviewBackend::Hydrolysis => {
            if platform.hydrolysis_target_platform().is_none() {
                bail!(
                    "Internal error: Hydrolysis preview backend is not supported on {platform:?}"
                );
            }
            toolchain_checks::check_hydrolysis(&host).await?;
        }
    }
    Ok(())
}

/// Render `symbol` through the support-app session, translating a missing
/// export into an actionable `#[preview]` hint.
///
/// # Errors
/// Returns an error if the preview app rejects the render or the transport
/// fails.
pub async fn render_with_symbol(
    session: &mut PreviewSession,
    function_path: &str,
    symbol: &str,
    dylib_id: DylibId,
    dylib_path: &std::path::Path,
    width: f32,
    height: f32,
) -> Result<Vec<u8>> {
    let prefer_local_path = session.platform == PreviewPlatform::Macos;
    match session
        .client
        .render_with_dylib_file(
            dylib_id,
            dylib_path,
            symbol,
            width,
            height,
            prefer_local_path,
        )
        .await
    {
        Ok(data) => Ok(data),
        Err(AppError::SymbolNotFound(_)) => {
            bail!("{}", missing_preview_symbol_message(function_path, symbol));
        }
        Err(err) => {
            bail!("Preview app error: {err}");
        }
    }
}

fn missing_preview_symbol_message(function_path: &str, symbol: &str) -> String {
    format!(
        "Preview component not found: `{function_path}`\nExpected export symbol: `{symbol}`\n\
The preview function is likely missing `#[preview]` (or the name is wrong).\n\
Example:\n  #[preview]\n  fn {}() -> impl View {{ ... }}",
        function_path.rsplit("::").next().unwrap_or(function_path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_preview_platform_selects_the_host_platform() {
        assert_eq!(
            native_preview_platform_for_os("macos"),
            Some(CliPreviewPlatform::Macos)
        );
        assert_eq!(
            native_preview_platform_for_os("linux"),
            Some(CliPreviewPlatform::Linux)
        );
        assert_eq!(
            native_preview_platform_for_os("windows"),
            Some(CliPreviewPlatform::Windows)
        );
        assert_eq!(native_preview_platform_for_os("freebsd"), None);
    }

    #[test]
    fn native_preview_platform_matches_this_host() {
        let expected = match std::env::consts::OS {
            "macos" => Some(CliPreviewPlatform::Macos),
            "linux" => Some(CliPreviewPlatform::Linux),
            "windows" => Some(CliPreviewPlatform::Windows),
            _ => None,
        };
        match expected {
            Some(platform) => {
                assert_eq!(resolve_preview_platform(None).unwrap(), platform);
            }
            None => assert!(resolve_preview_platform(None).is_err()),
        }
    }

    #[test]
    fn linux_and_windows_default_to_the_hydrolysis_backend() {
        for (platform, target) in [
            (CliPreviewPlatform::Linux, TargetPlatform::Linux),
            (CliPreviewPlatform::Windows, TargetPlatform::Windows),
        ] {
            assert_eq!(
                resolve_preview_backend(platform, None).unwrap(),
                CliPreviewBackend::Hydrolysis
            );
            assert_eq!(platform.hydrolysis_target_platform(), Some(target));
            assert_eq!(platform.support_app_platform(), None);
        }
    }

    #[test]
    fn hydrolysis_preview_theme_defaults_to_material3() {
        assert_eq!(
            resolve_hydrolysis_preview_theme(CliPreviewBackend::Hydrolysis, None).unwrap(),
            Some(HydrolysisPreviewTheme::Material3)
        );
        assert!(
            resolve_hydrolysis_preview_theme(
                CliPreviewBackend::Apple,
                Some(CliHydrolysisPreviewTheme::Material3)
            )
            .is_err()
        );
    }

    #[test]
    fn formats_missing_preview_symbol_message() {
        let symbol = "waterui_preview_app_card_preview";
        let message = missing_preview_symbol_message("dashboard::admin::card_preview", symbol);
        assert!(message.contains("dashboard::admin::card_preview"));
        assert!(message.contains("waterui_preview_app_card_preview"));
        assert!(message.contains("#[preview]"));
        assert!(message.contains("fn card_preview()"));
    }
}
