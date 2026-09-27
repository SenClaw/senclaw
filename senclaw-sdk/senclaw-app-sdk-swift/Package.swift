// swift-tools-version:5.9
import PackageDescription

// swift-tools-version 5.9 pins the Swift 5 language mode on purpose: a
// thread-per-connection socket server and a semaphore-blocking HTTP client are
// natural here, and Swift 6's strict-concurrency checking would demand actor
// ceremony that buys a loopback app server nothing.
let package = Package(
    name: "SenclawSpace",
    platforms: [.macOS(.v12)],
    products: [
        .library(name: "SenclawSpace", targets: ["SenclawSpace"]),
        .executable(name: "senclaw-manifest", targets: ["senclaw-manifest"]),
    ],
    targets: [
        // Foundation only — no third-party dependency, so an app that depends on
        // this has nothing to resolve and no install step before its first launch.
        .target(name: "SenclawSpace"),
        .executableTarget(name: "senclaw-manifest", dependencies: ["SenclawSpace"]),
        .testTarget(name: "SenclawSpaceTests", dependencies: ["SenclawSpace"]),
    ]
)
