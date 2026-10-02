//! Native FFI companion crate for {{ ctx.app_display_name }}.

use waterui::app::App;
use waterui::env::Environment;

// `export!()` expands `waterui_app`, which calls `app(env)` on every backend —
// the shim is not an Apple piece even though `export_app!` also consumes it.
fn app(env: Environment) -> App {
    {{ ctx.crate_name_ident() }}::app(env)
}

waterui_ffi::export!();
{% if ctx.apple_backend_selected %}
waterui_apple::export_app!(app);
{% endif %}

{% if ctx.cef_runtime_enabled() %}
// Re-exported so the entry-owning bin can call it through the lib
// dependency, the same path `waterui_apple_main` takes.
#[cfg(target_os = "macos")]
pub use waterui_ffi::components::platform::browser_cef::waterui_cef_prepare_macos_application;
{% endif %}
