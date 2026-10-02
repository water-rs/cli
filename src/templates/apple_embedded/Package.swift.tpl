// swift-tools-version: 6.3
import PackageDescription
{% if self.needs_clang_runtime() %}
import Foundation

// Resolve on the consuming host, using its selected Xcode toolchain.
// The package must not record the machine that assembled the Rust archive.
let clangRuntimeLibraryDirectory: String = {
    let clang = Process()
    clang.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
    clang.arguments = ["clang", "-print-resource-dir"]
    let output = Pipe()
    clang.standardOutput = output
    do {
        try clang.run()
    } catch {
        fatalError("Cannot discover the selected Clang runtime directory: \(error)")
    }
    let data = output.fileHandleForReading.readDataToEndOfFile()
    clang.waitUntilExit()
    guard clang.terminationReason == .exit, clang.terminationStatus == 0,
          let raw = String(data: data, encoding: .utf8) else {
        fatalError("xcrun clang -print-resource-dir failed")
    }
    let resourceDirectory = raw.trimmingCharacters(in: .whitespacesAndNewlines)
    guard resourceDirectory.hasPrefix("/") else {
        fatalError("Clang reported an invalid resource directory: \(resourceDirectory)")
    }
    let directory = URL(fileURLWithPath: resourceDirectory)
        .appendingPathComponent("lib/darwin").path(percentEncoded: false)
    var isDirectory: ObjCBool = false
    guard FileManager.default.fileExists(atPath: directory, isDirectory: &isDirectory),
          isDirectory.boolValue else {
        fatalError("The selected toolchain has no Darwin runtime directory: \(directory)")
    }
    return directory
}()
{% endif %}

let package = Package(
    name: "{{ name }}",
    platforms: [.macOS("{{ macos }}"), .iOS("{{ ios }}")],
    products: [.library(name: "WaterUI", targets: ["WaterUI"])],
    targets: [
        .binaryTarget(name: "WaterUINative", path: "WaterUINative.xcframework"),
        .target(
            name: "WaterUI",
            dependencies: ["WaterUINative"],
            resources: [.copy("Resources/waterui_assets"), .copy("Resources/fonts")],
            swiftSettings: [
                .enableExperimentalFeature("Extern"),
            ],
            linkerSettings: [
                .unsafeFlags([
                    "-Xlinker", "-u", "-Xlinker", "_waterui_apple_runtime_create",
                    "-Xlinker", "-u", "-Xlinker", "_waterui_apple_runtime_drop",
                    "-Xlinker", "-u", "-Xlinker", "_waterui_apple_mount",
                    "-Xlinker", "-u", "-Xlinker", "_waterui_apple_mount_drop",
                ]),
{% for platform in links %}
{% if platform.needs_clang_runtime() %}
                .unsafeFlags(["-L", clangRuntimeLibraryDirectory], .when(platforms: [.{{ platform.platform }}])),
{% endif %}
{% for link in platform.links %}
{% if link.framework %}
                .linkedFramework("{{ link.name }}", .when(platforms: [.{{ platform.platform }}])),
{% else %}
                .linkedLibrary("{{ link.name }}", .when(platforms: [.{{ platform.platform }}])),
{% endif %}
{% endfor %}
{% endfor %}
            ]
        ),
    ]
)
