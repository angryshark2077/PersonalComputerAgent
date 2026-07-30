import Darwin
import Dispatch
import Foundation
@testable import PlatformBridge

private struct HarnessCredentialProvider: BridgeCredentialProviding {
    func loadSecret() throws -> Data? {
        Data(repeating: 0x5a, count: 32)
    }
}

guard CommandLine.arguments.count == 7,
      CommandLine.arguments[1] == "--socket",
      CommandLine.arguments[3] == "--run-root",
      CommandLine.arguments[5] == "--ready-hook" else {
    exit(2)
}

let socketURL = URL(fileURLWithPath: CommandLine.arguments[2])
let runRoot = URL(fileURLWithPath: CommandLine.arguments[4], isDirectory: true)
let readyHook = URL(fileURLWithPath: CommandLine.arguments[6])

do {
    try TerminationSignalWaiter.block()
    let coordinator = TerminationSignalCoordinator()
    TerminationSignalWaiter.start(coordinator: coordinator)
    guard FileManager.default.createFile(atPath: readyHook.path, contents: Data("ready".utf8)) else {
        exit(3)
    }

    // The hook deliberately opens the earliest controlled startup window after signal masking.
    usleep(300_000)
    Task {
        let server = BridgeServer(
            socketURL: socketURL,
            pathValidator: SocketPathValidator(approvedRunRoot: runRoot),
            handshakeHandler: HandshakeHandler(bridgeVersion: "signal-harness"),
            credentialProvider: HarnessCredentialProvider()
        )
        do {
            guard try await coordinator.register(server: server) else { exit(0) }
            try await server.start()
            try await server.serve()
            try await server.shutdown()
            exit(0)
        } catch BridgeServerError.shutdownRequested {
            exit(0)
        } catch {
            exit(4)
        }
    }
    dispatchMain()
} catch {
    exit(5)
}
