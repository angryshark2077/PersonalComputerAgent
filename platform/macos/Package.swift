// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "PCAPlatformMacOS",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "BridgeProtocol", targets: ["BridgeProtocol"]),
        .executable(name: "SetupAppPlaceholder", targets: ["SetupAppPlaceholder"])
    ],
    targets: [
        .target(name: "BridgeProtocol"),
        .executableTarget(
            name: "SetupAppPlaceholder",
            dependencies: ["BridgeProtocol"]
        )
    ]
)
