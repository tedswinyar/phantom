// swift-tools-version: 6.0
import PackageDescription

// Swift 6 language mode is the enforced bar for this template: complete
// concurrency checking is ON by default, so data races are compile errors,
// not TSan surprises. Every target opts in explicitly so a future edit that
// drops the package tools-version can't silently relax the bar.
let swift6: [SwiftSetting] = [.swiftLanguageMode(.v6)]

let package = Package(
    name: "Phantom",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "PhantomCore", targets: ["PhantomCore"]),
        .library(name: "DesignKit", targets: ["DesignKit"]),
    ],
    dependencies: [
        // Auto-update (phantom-pxt). Sparkle 2 ships as a binary XCFramework;
        // build-app.sh embeds it in Contents/Frameworks and re-signs it.
        .package(url: "https://github.com/sparkle-project/Sparkle", from: "2.6.0"),
    ],
    targets: [
        // Design tokens — colors, spacing, typography. No app logic.
        .target(
            name: "DesignKit",
            path: "Sources/DesignKit",
            swiftSettings: swift6
        ),
        // Shared library — API client, wire models, server supervision, and
        // the NotesModel view model (here, not in the app target, so it is
        // unit-testable at the network boundary).
        .target(
            name: "PhantomCore",
            path: "Sources/PhantomCore",
            swiftSettings: swift6
        ),
        // macOS SwiftUI app.
        .executableTarget(
            name: "Phantom",
            dependencies: [
                "PhantomCore",
                "DesignKit",
                .product(name: "Sparkle", package: "Sparkle"),
            ],
            path: "Sources/Phantom",
            swiftSettings: swift6
        ),
        .testTarget(
            name: "PhantomCoreTests",
            dependencies: ["PhantomCore"],
            path: "Tests/PhantomCoreTests",
            swiftSettings: swift6
        ),
        .testTarget(
            name: "DesignKitTests",
            dependencies: ["DesignKit"],
            path: "Tests/DesignKitTests",
            swiftSettings: swift6
        ),
    ]
)
