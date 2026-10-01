//! Compile the backend's Swift seam into a static archive.
//!
//! `Sources/WaterUI` in the Apple backend checkout carries the `@_cdecl`
//! `waterui_swift_*` entry points `waterui-apple` calls across the Rust/Swift
//! seam. The generated Xcode project used to compile them as a Swift package
//! target; entry-owning packaging compiles them itself with `swiftc` — no
//! `swift build`, no `xcodebuild` — and links the archive into the application
//! executable. `debug` mirrors the Cargo profile so the Swift `#if DEBUG`
//! exports exist exactly when `debug_assertions` code needs them.
//!
//! The ffi library itself does not link the archive: a `cdylib` it produces is
//! loaded by a host that already provides the seam (the executable), so its
//! link line keeps `-undefined dynamic_lookup` exactly like the old host-app
//! contract.

#[cfg(target_os = "macos")]
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use askama::Template as _;
use color_eyre::eyre::bail;
#[cfg(target_os = "macos")]
use color_eyre::eyre::{Context, eyre};
#[cfg(target_os = "macos")]
use smol::fs;
#[cfg(target_os = "macos")]
use tracing::info;

use crate::platform::TargetPlatform;
#[cfg(target_os = "macos")]
use crate::utils::run_command_os;

/// Askama escaper for the quoted header paths in `CWaterUI.modulemap`: a
/// module map string literal escapes `"` and `\` with a backslash.
#[derive(Clone, Copy, Debug)]
pub struct ModuleMapEscape;

impl askama::filters::Escaper for ModuleMapEscape {
    fn write_escaped_str<W: std::fmt::Write>(&self, mut dest: W, string: &str) -> std::fmt::Result {
        for c in string.chars() {
            if matches!(c, '"' | '\\') {
                dest.write_char('\\')?;
            }
            dest.write_char(c)?;
        }
        Ok(())
    }
}

/// The `CWaterUI` module map that hands the seam's C headers to `swiftc`
/// (`-fmodule-map-file`) so they resolve without an SPM target.
#[derive(askama::Template)]
#[template(path = "apple/CWaterUI.modulemap")]
struct CWaterUiModuleMap<'a> {
    headers: &'a [String],
}

/// The `@_cdecl` symbols `Sources/WaterUI` exports across the Rust/Swift
/// seam — the archive resolves them for the executable, a `cdylib` keeps
/// them explicitly undefined for the host to provide.
pub const SEAM_SYMBOL_NAMES: &[&str] = &[
    "waterui_swift_claims",
    "waterui_swift_content_frame",
    "waterui_swift_install_webview",
    "waterui_swift_manages_safe_area",
    "waterui_swift_prepare_env",
    "waterui_swift_render",
    "waterui_swift_safe_area_rect",
    "waterui_swift_when_ready",
];

/// The symbols the application exports back across the seam.
///
/// `export_app!` defines `waterui_init`/`waterui_app` in the ffi rlib and
/// `entry::run` calls them, so the entry link marks them explicitly
/// undefined (`-Wl,-u`) to keep their archive members on the image.
pub const APP_SEAM_CALLBACK_SYMBOLS: &[&str] = &["waterui_init", "waterui_app"];

/// Compile `backend_root/Sources/WaterUI` into `out_dir/libWaterUISwift.a`
/// for the target `platform` builds at `deployment_target`.
///
/// `defines` are forwarded as `-D` conditional-compilation flags
/// (`WATERUI_MAP`, `WATERUI_WEBVIEW`, `WATERUI_NO_MEDIA`, `WATERUI_NO_GPU`)
/// so the archive carries exactly the C surface the application's graph
/// links.
///
/// Returns the archive path. Recompiles on every call — `swiftc` has no
/// cheap incremental mode, and the build cache is per-project anyway.
///
/// # Errors
/// Returns an error when the backend has no seam sources, the SDK cannot be
/// resolved, or `swiftc` fails.
#[cfg(target_os = "macos")]
pub async fn compile_swift_seam(
    backend_root: &Path,
    platform: TargetPlatform,
    deployment_target: &str,
    debug: bool,
    defines: &[String],
    out_dir: &Path,
) -> eyre::Result<PathBuf> {
    let swift_dir = backend_root.join("Sources/WaterUI");
    let mut sources = Vec::new();
    collect_swift_sources(&swift_dir, &mut sources).await?;
    if sources.is_empty() {
        bail!(
            "Apple backend at {} has no Swift seam sources under Sources/WaterUI",
            backend_root.display()
        );
    }
    sources.sort();

    let sdk_name = platform
        .sdk_name()
        .ok_or_else(|| eyre!("Platform {platform:?} is not an Apple platform"))?;
    let sdk_path = run_command_os(
        "xcrun",
        ["--sdk", sdk_name, "--show-sdk-path"].map(OsString::from),
    )
    .await
    .map(|stdout| stdout.trim().to_string())
    .wrap_err("xcrun could not resolve the SDK path")?;
    if sdk_path.is_empty() {
        bail!("xcrun --sdk {sdk_name} --show-sdk-path returned an empty path");
    }

    // `CWaterUI` is a header-only module (its lone .c file is an SPM stub), so
    // a generated modulemap hands it to swiftc directly.
    fs::create_dir_all(out_dir).await?;
    let include_dir = backend_root.join("Sources/CWaterUI/include");
    let modulemap = out_dir.join("CWaterUI.modulemap");
    let headers = ["waterui.h", "waterui_seam.h", "waterkit_audio_apple.h"]
        .iter()
        .map(|header| {
            let path = include_dir.join(header);
            if !path.is_file() {
                bail!(
                    "Apple backend is missing CWaterUI header {}",
                    path.display()
                );
            }
            Ok(path.to_string_lossy().into_owned())
        })
        .collect::<eyre::Result<Vec<String>>>()?;
    let mut modulemap_contents = CWaterUiModuleMap { headers: &headers }
        .render()
        .wrap_err("failed to render the CWaterUI module map")?;
    modulemap_contents.push('\n');
    fs::write(&modulemap, modulemap_contents).await?;

    let archive = out_dir.join("libWaterUISwift.a");
    let mut args = vec![
        "swiftc".into(),
        "-emit-library".into(),
        "-static".into(),
        "-o".into(),
        archive.as_os_str().to_os_string(),
        "-module-name".into(),
        "WaterUI".into(),
        "-parse-as-library".into(),
        "-target".into(),
        swift_target_triple(platform, deployment_target)?,
        "-sdk".into(),
        sdk_path.into(),
        "-Xcc".into(),
        format!("-fmodule-map-file={}", modulemap.display()).into(),
    ];
    // `#if DEBUG` gates the debug-only seam exports (`waterui_swift_claims`)
    // the Rust side references under `debug_assertions`; mirror the Cargo
    // profile into the archive.
    if debug {
        args.push("-D".into());
        args.push("DEBUG".into());
    } else {
        args.push("-O".into());
        args.push("-whole-module-optimization".into());
    }
    for define in defines {
        args.push("-D".into());
        args.push(define.clone().into());
    }
    args.extend(sources.iter().map(|path| path.as_os_str().to_os_string()));
    run_command_os("xcrun", args)
        .await
        .wrap_err("swiftc failed to compile the backend's Swift seam")?;
    info!(archive = %archive.display(), "Compiled the Swift seam archive");
    Ok(archive)
}

/// Non-macOS hosts cannot run `swiftc`; Apple packaging only ever ran on
/// macOS (it drove `xcodebuild` before), so the check is a plain error.
///
/// # Errors
/// Always fails: the Swift toolchain exists only on macOS hosts.
#[cfg(not(target_os = "macos"))]
#[expect(
    clippy::unused_async,
    reason = "keeps the signature of the macOS implementation, which awaits"
)]
pub async fn compile_swift_seam(
    _backend_root: &Path,
    _platform: TargetPlatform,
    _deployment_target: &str,
    _debug: bool,
    _defines: &[String],
    _out_dir: &Path,
) -> eyre::Result<PathBuf> {
    bail!("Apple packaging requires macOS (swiftc is part of the Xcode toolchain)")
}

/// The `-target` triple `swiftc` expects for `platform` at
/// `deployment_target` (`arm64-apple-ios26.0-simulator` style).
#[cfg(target_os = "macos")]
fn swift_target_triple(
    platform: TargetPlatform,
    deployment_target: &str,
) -> eyre::Result<std::ffi::OsString> {
    let (os, simulator) = match platform {
        TargetPlatform::MacOS => ("macos", false),
        TargetPlatform::IOS => ("ios", false),
        TargetPlatform::IOSSimulator => ("ios", true),
        TargetPlatform::TvOS => ("tvos", false),
        TargetPlatform::TvOSSimulator => ("tvos", true),
        TargetPlatform::WatchOS => ("watchos", false),
        TargetPlatform::WatchOSSimulator => ("watchos", true),
        TargetPlatform::VisionOS => ("xros", false),
        TargetPlatform::VisionOSSimulator => ("xros", true),
        platform => bail!("Platform {platform:?} is not an Apple platform"),
    };
    let arch = swift_arch(platform.triple().architecture)?;
    let suffix = if simulator { "-simulator" } else { "" };
    Ok(format!("{arch}-apple-{os}{deployment_target}{suffix}").into())
}

/// The `swiftc -target` arch segment for a Rust triple's architecture.
#[cfg(target_os = "macos")]
fn swift_arch(arch: target_lexicon::Architecture) -> eyre::Result<&'static str> {
    match arch {
        target_lexicon::Architecture::Aarch64(_) => Ok("arm64"),
        target_lexicon::Architecture::X86_64 => Ok("x86_64"),
        arch => bail!("Apple packaging does not support the {arch} architecture"),
    }
}

#[cfg(target_os = "macos")]
async fn collect_swift_sources(dir: &Path, out: &mut Vec<PathBuf>) -> eyre::Result<()> {
    use smol::stream::StreamExt as _;
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            Box::pin(collect_swift_sources(&path, out)).await?;
        } else if path.extension().is_some_and(|ext| ext == "swift") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use askama::Template as _;

    #[test]
    fn app_seam_callback_symbols_are_the_export_app_contract() {
        assert_eq!(
            super::APP_SEAM_CALLBACK_SYMBOLS,
            ["waterui_init", "waterui_app"]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_target_triple_maps_the_supported_arch_arms() {
        let arch_of = |triple: &str| {
            triple
                .parse::<target_lexicon::Triple>()
                .expect("a Rust triple")
                .architecture
        };
        assert_eq!(
            super::swift_arch(arch_of("aarch64-apple-darwin")).expect("arm64 maps"),
            "arm64"
        );
        assert_eq!(
            super::swift_arch(arch_of("x86_64-apple-darwin")).expect("x86_64 maps"),
            "x86_64"
        );
        assert!(
            super::swift_arch(arch_of("armv7-unknown-linux-gnueabihf")).is_err(),
            "a non-Apple arch is an error, not a silent default"
        );
    }

    #[test]
    fn modulemap_escapes_header_paths_as_string_literals() {
        let headers = vec![
            "/tmp/with space/quo\"te.h".to_string(),
            "C:\\sdk\\include\\waterui.h".to_string(),
        ];
        let rendered = super::CWaterUiModuleMap { headers: &headers }
            .render()
            .expect("the module map renders");
        assert!(
            rendered.contains("    header \"/tmp/with space/quo\\\"te.h\"\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("    header \"C:\\\\sdk\\\\include\\\\waterui.h\"\n"),
            "{rendered}"
        );
        assert!(
            rendered.starts_with("module CWaterUI {\n") && rendered.ends_with("    export *\n}"),
            "{rendered:?}"
        );
    }

    #[test]
    fn modulemap_parse_checks_with_clang_when_present() {
        use std::ffi::OsString;

        smol::block_on(async {
            let host = crate::toolchain::Host::current();
            let Ok(clang) = host.which("clang").await else {
                return;
            };
            let dir = tempfile::tempdir().expect("a scratch dir");
            let include = dir.path().join("with space");
            std::fs::create_dir_all(&include).expect("the include dir");
            let dummy = include.join("dummy.h");
            std::fs::write(&dummy, "#pragma once\n").expect("the dummy header");
            let headers = vec![dummy.to_string_lossy().into_owned()];
            let rendered = super::CWaterUiModuleMap { headers: &headers }
                .render()
                .expect("the module map renders");
            let modulemap = dir.path().join("CWaterUI.modulemap");
            std::fs::write(&modulemap, &rendered).expect("the module map file");
            let empty = dir.path().join("empty.c");
            std::fs::write(&empty, "").expect("the empty source");
            let output = host
                .output(
                    &clang,
                    [
                        OsString::from("-fsyntax-only"),
                        OsString::from("-fmodules"),
                        OsString::from(format!("-fmodule-map-file={}", modulemap.display())),
                        OsString::from("-x"),
                        OsString::from("c"),
                        empty.as_os_str().to_os_string(),
                    ],
                )
                .await
                .expect("clang runs");
            assert!(
                output.status.success() && !output.stderr.windows(6).any(|w| w == b"error:"),
                "clang rejected the rendered module map: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_target_triple_marks_simulator_platforms() {
        let platform = crate::platform::TargetPlatform::IOSSimulator;
        let rendered =
            super::swift_target_triple(platform, "26.0").expect("a simulator triple renders");
        let rendered = rendered.to_str().expect("the triple is always valid UTF-8");
        let arch =
            super::swift_arch(platform.triple().architecture).expect("the host arch is supported");
        assert_eq!(rendered, format!("{arch}-apple-ios26.0-simulator").as_str());
    }
}
