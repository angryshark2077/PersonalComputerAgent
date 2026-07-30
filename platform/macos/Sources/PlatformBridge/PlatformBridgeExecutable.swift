import Darwin
import Dispatch
import Foundation

private enum BridgeArgumentsError: Error {
    case invalid
}

private struct BridgeArguments {
    let socketURL: URL

    static func parse(_ arguments: [String]) throws -> BridgeArguments {
        guard arguments.count == 3,
              arguments[1] == "--socket",
              arguments[2].hasPrefix("/") else {
            throw BridgeArgumentsError.invalid
        }
        return BridgeArguments(socketURL: URL(fileURLWithPath: arguments[2]))
    }
}

private func safeFailure(_ message: String) {
    FileHandle.standardError.write(Data("PCAPlatformBridge: \(message)\n".utf8))
}

private func runBridge(coordinator: TerminationSignalCoordinator) async -> Int32 {
    do {
        let arguments = try BridgeArguments.parse(CommandLine.arguments)
        let validator = SocketPathValidator()
        try validator.validate(socketURL: arguments.socketURL)
        let server = BridgeServer(
            socketURL: arguments.socketURL,
            pathValidator: validator,
            handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
            credentialProvider: KeychainBridgeCredentialProvider()
        )
        guard try await coordinator.register(server: server) else { return 0 }

        do {
            try await server.start()
        } catch BridgeServerError.shutdownRequested {
            return 0
        }

        let powerMonitor = await MainActor.run {
            let monitor = PowerMonitor { _ in
                // The typed callback is intentionally not serialized until the lifecycle wire schema is frozen.
            }
            monitor.start()
            return monitor
        }
        do {
            try await server.serve()
        } catch let serveError {
            do {
                try await server.shutdown()
            } catch {
                await MainActor.run { powerMonitor.stop() }
                throw error
            }
            await MainActor.run { powerMonitor.stop() }
            throw serveError
        }
        try await server.shutdown()
        await MainActor.run { powerMonitor.stop() }
        return 0
    } catch {
        safeFailure("startup or server failure")
        return 1
    }
}

public enum PlatformBridgeExecutable {
    public static func main() -> Never {
        do {
            try TerminationSignalWaiter.block()
        } catch {
            safeFailure("signal setup failure")
            exit(1)
        }
        let terminationCoordinator = TerminationSignalCoordinator()
        TerminationSignalWaiter.start(coordinator: terminationCoordinator)

        Task {
            exit(await runBridge(coordinator: terminationCoordinator))
        }
        dispatchMain()
    }
}
