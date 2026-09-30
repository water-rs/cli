//! The entry-owning Apple entry point for {{ ctx.app_display_name }}.
//!
//! This binary declares `waterui_apple_main` — which `waterui_apple::export_app!`
//! expanded inside the companion library — as an external symbol and calls it.
//! It references no crate, so its codegen units carry none of the `waterui_*`
//! exports: every Rust symbol in the executable comes from the one artifact the
//! packaging link threads (`lib{crate}_ffi.a`, or the ffi rlib plus the shared
//! runtime dylib under the dynamic linkage).

unsafe extern "C" {
    /// The application's entry, defined by the companion library. It owns the
    /// process startup, the environment, the declared windows and the platform
    /// run loop, and never returns.
    fn waterui_apple_main(accessory: bool);

    {% if ctx.cef_runtime_enabled() %}
    /// CEF installs its `NSApplication` subclass before AppKit creates the
    /// shared application, so this runs before `entry::run` touches
    /// `NSApplication.shared` — macOS only.
    #[cfg(target_os = "macos")]
    fn waterui_cef_prepare_macos_application();
    {% endif %}
}

fn main() -> ! {
    {% if ctx.cef_runtime_enabled() %}
    #[cfg(target_os = "macos")]
    unsafe {
        waterui_cef_prepare_macos_application();
    }
    {% endif %}
    // SAFETY: this is the process's entry on the main thread.
    unsafe {
        waterui_apple_main({{ ctx.accessory }});
    }
    unreachable!("waterui_apple_main never returns");
}
