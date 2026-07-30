// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "PCAPlatformMacOS",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "BridgeProtocol", targets: ["BridgeProtocol"]),
        .executable(name: "SetupAppPlaceholder", targets: ["SetupAppPlaceholder"]),
        .executable(name: "BridgeContractVerifier", targets: ["BridgeContractVerifier"]),
        .executable(name: "PCAPlatformBridge", targets: ["PCAPlatformBridge"])
    ],
    targets: [
        .target(
            name: "CSignalRelay",
            path: "Sources/CSignalRelay",
            publicHeadersPath: "include"
        ),
        .target(name: "BridgeProtocol"),
        .executableTarget(
            name: "SetupAppPlaceholder",
            dependencies: ["BridgeProtocol"]
        ),
        .executableTarget(
            name: "BridgeContractVerifier",
            dependencies: ["BridgeProtocol"]
        ),
        .target(
            name: "PlatformBridge",
            dependencies: ["BridgeProtocol", "CSignalRelay"]
        ),
        .executableTarget(
            name: "PCAPlatformBridge",
            dependencies: ["PlatformBridge"]
        ),
        .executableTarget(
            name: "PlatformBridgeSignalHarness",
            dependencies: ["PlatformBridge"],
            path: "Tests/PlatformBridgeSignalHarness"
        ),
        .testTarget(
            name: "PlatformBridgeTests",
            dependencies: ["PlatformBridge", "PlatformBridgeSignalHarness", "BridgeProtocol"]
        ),
    ]
)
