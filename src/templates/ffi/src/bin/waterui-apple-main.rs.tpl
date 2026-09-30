//! The entry-owning Apple entry point for {{ ctx.app_display_name }}.
//!
//! This is the same wiring `waterui_apple::export_app!` expands to in
//! `lib.rs`; the library keeps that expansion for the embedding path while
//! this binary owns the process entry itself — no Swift host involved.

use waterui::app::App;
use waterui::env::Environment;

// Delegating through the library keeps its rlib on the link line: rustc
// drops an unused `--extern`, and the Swift seam resolves `waterui_init` /
// `waterui_app` out of that archive during the normal left-to-right pass —
// with no second, graph-bundling staticlib anywhere on the line.
fn app(env: Environment) -> App {
    {{ ctx.ffi_crate_ident() }}::app(env)
}

fn main() -> ! {
    let mut env = waterui::configure_environment!(waterui::Environment::new());
    waterui_ffi::__configure_native_realizations(&mut env);
    // SAFETY: this is the process's entry on the main thread, and `env`
    // lives in this frame — `run` never returns.
    unsafe {
        waterui_apple::entry::run(app, &mut env, {{ ctx.accessory }});
    }
}
