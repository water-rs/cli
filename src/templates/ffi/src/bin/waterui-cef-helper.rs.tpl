//! Minimal CEF subprocess entry point for {{ ctx.app_display_name }}.

fn main() {
    #[cfg(target_os = "macos")]
    std::process::exit(waterui_browser_cef::run_packaged_subprocess());
    #[cfg(not(target_os = "macos"))]
    std::process::exit(
        waterui_ffi::components::platform::browser_cef::waterui_cef_run_packaged_subprocess(),
    );
}
