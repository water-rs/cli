//! The entry-owning Apple entry point for {{ ctx.app_display_name }}.
//!
//! This is the same wiring `waterui_apple::export_app!` expands to in
//! `lib.rs`; the library keeps that expansion for the embedding path while
//! this binary owns the process entry itself — no Swift host involved.

use waterui::app::App;
use waterui::env::Environment;

fn app(env: Environment) -> App {
    {{ ctx.crate_name_ident() }}::app(env)
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
