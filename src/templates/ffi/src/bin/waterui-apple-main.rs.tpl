//! The entry-owning Apple entry point for {{ ctx.app_display_name }}.
//!
//! This is the same wiring `waterui_apple::export_app!` expands to in
//! `lib.rs`; the library keeps that expansion for the embedding path while
//! this binary owns the process entry itself — no Swift host involved.

use waterui::app::App;
use waterui::env::Environment;

fn app(mut env: Environment) -> App {
    // The realizations this backend brings run inside `entry::run`'s launch
    // handler so `spawn_local` users such as the CEF message pump see the
    // local executor `run` installs at startup — and they still land on the
    // environment before the application installs its own.
    waterui_ffi::__configure_native_realizations(&mut env);
    {{ ctx.crate_name_ident() }}::app(env)
}

fn main() -> ! {
    {% if ctx.cef_runtime_enabled() %}
    // CEF must install its NSApplication subclass before AppKit creates it;
    // this runs before `entry::run` touches `NSApplication.shared`.
    #[cfg(target_os = "macos")]
    waterui_ffi::components::platform::browser_cef::waterui_cef_prepare_macos_application();
    {% endif %}
    let mut env = waterui::configure_environment!(waterui::Environment::new());
    // SAFETY: this is the process's entry on the main thread, and `env`
    // lives in this frame — `run` never returns.
    unsafe {
        waterui_apple::entry::run(app, &mut env, {{ ctx.accessory }});
    }
}
