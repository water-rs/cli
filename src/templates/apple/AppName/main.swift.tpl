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
{% if ctx.cef_runtime_enabled() %}

// CEF must initialize before NSApplication exists; `waterui_apple_main`
// owns the application run loop, so the browser-process hooks stay here.
prepareWaterUICEFApplication()
{% endif %}
{% if ctx.chromium_enabled() %}
installWaterUIChromium()
{% endif %}
{% if ctx.cef_webview_enabled() %}
installWaterUICefWebView()
{% endif %}
waterui_apple_main({{ ctx.accessory }})
