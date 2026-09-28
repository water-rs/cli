#if os(iOS)
import Darwin
import UIKit
import WaterUI

@main
class AppDelegate: UIResponder, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        // Register custom fonts from dependencies
        WaterUIFonts.register()
        if let assetsRoot = Bundle.main.resourceURL?.appendingPathComponent("waterui_assets").path {
            setenv("WATERUI_ASSETS_ROOT", assetsRoot, 1)
        }
        return true
    }
}

// The window belongs to the scene, not the app: UIKit requires the scene
// life cycle from the iOS 27 SDK on, and Info.plist names this class (by its
// Objective-C name, so the module name stays out of the manifest) as the
// delegate of the app's single window scene.
@objc(SceneDelegate)
class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = scene as? UIWindowScene else {
            fatalError("The application scene is not a window scene: \(scene)")
        }
        let window = UIWindow(windowScene: windowScene)
        window.rootViewController = WaterUIViewController()
        window.makeKeyAndVisible()
        self.window = window
    }
}
#elseif os(macOS)
import Darwin
import AppKit
import WaterUI
{% if ctx.cef_runtime_enabled() %}
import WaterUICEF
{% endif %}
{% if ctx.chromium_enabled() %}
import WaterUIChromium
{% endif %}
{% if ctx.cef_webview_enabled() %}
import WaterUICefWebView
{% endif %}

@main
class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow?
    private var context: WuiRootContext?
    private var launchTask: Task<Void, Never>?
    private let isAccessory: Bool = {{ ctx.accessory }}

    static func main() {
{% if ctx.cef_runtime_enabled() %}
        prepareWaterUICEFApplication()
{% endif %}
{% if ctx.chromium_enabled() %}
        installWaterUIChromium()
{% endif %}
{% if ctx.cef_webview_enabled() %}
        installWaterUICefWebView()
{% endif %}
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        if delegate.isAccessory {
            app.setActivationPolicy(.prohibited)
        } else {
            app.mainMenu = WaterUIMainMenu.create()
        }
        app.run()
    }



    func applicationDidFinishLaunching(_ notification: Notification) {
        // Register custom fonts from dependencies
        WaterUIFonts.register()
        if let assetsRoot = Bundle.main.resourceURL?.appendingPathComponent("waterui_assets").path {
            setenv("WATERUI_ASSETS_ROOT", assetsRoot, 1)
        }

        // The runtime starts before any window: the application decides
        // whether it has one to show, and what happens once none is left.
        launchTask = Task { @MainActor [weak self] in
            let context = await WuiRootContext()
            guard let self, !Task.isCancelled else { return }
            self.context = context
            if self.isAccessory {
                // Force view construction so Preview::body runs and TCP server starts.
                _ = context.rootView
                return
            }
            guard context.window != nil else {
                // An application with no window either stays resident or has
                // nothing to run.
                if context.terminatesAfterLastWindowClosed {
                    NSApp.terminate(nil)
                }
                return
            }
            let window = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 800, height: 600),
                styleMask: [.titled, .closable, .miniaturizable, .resizable],
                backing: .buffered,
                defer: false
            )
            window.title = "{{ ctx.app_display_name }}"
            window.contentView = WaterUIView(
                context: context,
                frame: window.contentRect(forFrameRect: window.frame)
            )
            window.center()
            window.makeKeyAndOrderFront(nil)
            self.window = window
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        // No window opens before the runtime has started, so a window closing
        // before then is none of the application's.
        guard !isAccessory, let context else { return false }
        return context.terminatesAfterLastWindowClosed
    }

    func applicationWillTerminate(_ notification: Notification) {
        launchTask?.cancel()
        launchTask = nil
        context = nil
    }
}
#endif
