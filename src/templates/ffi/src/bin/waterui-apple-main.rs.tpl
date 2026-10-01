//! The entry-owning Apple entry point for {{ ctx.app_display_name }}.
//!
//! This binary calls `waterui_apple_main` — which `waterui_apple::export_app!`
//! expanded inside the companion library — through the package's own lib
//! dependency. Depending on the library keeps every `waterui_*` export and
//! every `#[link]` declaration in its crate graph on this executable's link:
//! the native dependencies the graph declares reach the link line through
//! crate metadata, which a bare archive input would drop.

use {{ ctx.ffi_crate_ident() }}::waterui_apple_main;
{% if ctx.cef_runtime_enabled() %}
/// CEF installs its `NSApplication` subclass before AppKit creates the
/// shared application, so this runs before `entry::run` touches
/// `NSApplication.shared` — macOS only.
#[cfg(target_os = "macos")]
use {{ ctx.ffi_crate_ident() }}::waterui_cef_prepare_macos_application;
{% endif %}

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
