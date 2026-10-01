//! Native FFI companion crate for {{ ctx.app_display_name }}.

use waterui::app::App;
use waterui::env::Environment;

pub fn app(env: Environment) -> App {
    {{ ctx.crate_name_ident() }}::app(env)
}

waterui_ffi::export!();
{% if ctx.apple_backend_selected %}
waterui_apple::export_app!(app);
{% endif %}
