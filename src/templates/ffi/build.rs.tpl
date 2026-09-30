//! Passes the application's `i18n/` directory to `catalog!`/`text!`.

fn main() {
{% include "partials/build_script_i18n.rs.tpl" %}

    // Entry-owning Apple packaging links this crate's `cdylib` twice over: once
    // as the `--lib` unit and once as a dependency artifact of `cargo rustc
    // --bin`, which never receives the build's trailing rustc args. The crate
    // therefore declares the backend's Swift-seam symbols explicitly undefined
    // itself, so every cdylib link keeps the host-provides-the-seam contract.
    if std::env::var("TARGET").is_ok_and(|t| t.contains("-apple-")) {
        for symbol in [
            "waterui_swift_claims",
            "waterui_swift_content_frame",
            "waterui_swift_install_webview",
            "waterui_swift_manages_safe_area",
            "waterui_swift_prepare_env",
            "waterui_swift_render",
            "waterui_swift_safe_area_rect",
            "waterui_swift_when_ready",
        ] {
            println!("cargo:rustc-link-arg-cdylib=-Wl,-U,_{symbol}");
        }
    }
}
