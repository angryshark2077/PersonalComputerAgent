// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "PCAPlatformMacOS",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "BridgeProtocol", targets: ["BridgeProtocol"]),
        .executable(name: "SetupAppPlaceholder", targets: ["SetupAppPlaceholder"]),
        .executable(name: "BridgeContractVerifier", targets: ["BridgeContractVerifier"]),
        .executable(name: "PCAPlatformBridge", targets: ["PlatformBridge"])
    ],
    targets: [
        .target(name: "BridgeProtocol"),
        .executableTarget(
            name: "SetupAppPlaceholder",
            dependencies: ["BridgeProtocol"]
        ),
        .executableTarget(
            name: "BridgeContractVerifier",
            dependencies: ["BridgeProtocol"]
        ),
        .executableTarget(
            name: "PlatformBridge",
            dependencies: ["BridgeProtocol"]
        ),
        .testTarget(
            name: "PlatformBridgeTests",
            dependencies: ["PlatformBridge", "BridgeProtocol"]
        ),
    ]
)
