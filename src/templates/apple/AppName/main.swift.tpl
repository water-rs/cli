import CWaterUI
{% if ctx.cef_runtime_enabled() %}
import WaterUICEF
{% endif %}
{% if ctx.chromium_enabled() %}
import WaterUIChromium
{% endif %}
{% if ctx.cef_webview_enabled() %}
import WaterUICefWebView
{% endif %}
{% if ctx.cef_any_enabled() %}

// CEF must initialize before NSApplication exists; `waterui_apple_main`
// owns the application run loop, so the browser-process hooks stay here.
// The hooks are @MainActor-isolated and the process entry runs on the
// main thread, so assumeIsolated is a checked zero-cost hand-off.
MainActor.assumeIsolated {
{% if ctx.cef_runtime_enabled() %}
    prepareWaterUICEFApplication()
{% endif %}
{% if ctx.chromium_enabled() %}
    installWaterUIChromium()
{% endif %}
{% if ctx.cef_webview_enabled() %}
    installWaterUICefWebView()
{% endif %}
}
{% endif %}
waterui_apple_main({{ ctx.accessory }})
