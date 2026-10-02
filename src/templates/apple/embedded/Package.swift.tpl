// swift-tools-version: 6.3
import PackageDescription

let package = Package(
    name: "{{ name }}",
    platforms: [.macOS("{{ macos }}"), .iOS("{{ ios }}")],
    products: [.library(name: "WaterUI", targets: ["WaterUI"])],
    targets: [
        .binaryTarget(name: "CWaterUI", path: "CWaterUI.xcframework"),
        .target(
            name: "WaterUI",
            dependencies: ["CWaterUI"],
            resources: [.copy("Resources/waterui_assets"), .copy("Resources/fonts")],
            swiftSettings: [
                .define("WATERUI_EMBEDDED_RESOURCES"),
{% for define in defines %}
                .define("{{ define }}"),
{% endfor %}
            ],
            linkerSettings: [
{% for platform in links %}
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
