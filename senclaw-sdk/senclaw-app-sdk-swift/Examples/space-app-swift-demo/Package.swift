// swift-tools-version:5.9
import PackageDescription

// A standalone package that depends on the SDK by path. An app outside this
// monorepo points the dependency at the git URL / a tagged version instead:
//
//     .package(url: "https://github.com/NortonBen/SenClaw.git", from: "0.1.0")
//
let package = Package(
    name: "space-app-swift-demo",
    platforms: [.macOS(.v12)],
    dependencies: [
        .package(name: "SenclawSpace", path: "../..")
    ],
    targets: [
        .executableTarget(
            name: "space-app-swift-demo",
            dependencies: [.product(name: "SenclawSpace", package: "SenclawSpace")]
        )
    ]
)
