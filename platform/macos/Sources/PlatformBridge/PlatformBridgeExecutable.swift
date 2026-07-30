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

public enum PlatformBridgeExecutable {
    static func run(
        arguments commandLineArguments: [String],
        signalRuntime: TerminationSignalRuntime
    ) async -> Int32 {
        if signalRuntime.terminationAcceptedOrPending() { return 0 }
        do {
            let arguments = try BridgeArguments.parse(commandLineArguments)
            let validator = SocketPathValidator()
            try validator.validate(socketURL: arguments.socketURL)
            let server = BridgeServer(
                socketURL: arguments.socketURL,
                pathValidator: validator,
                handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
                credentialProvider: KeychainBridgeCredentialProvider()
            )
            guard try await signalRuntime.coordinator.register(server: server) else { return 0 }

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
            if signalRuntime.terminationAcceptedOrPending() { return 0 }
            safeFailure("startup or server failure")
            return 1
        }
    }

    public static func main() -> Never {
        let signalRuntime: TerminationSignalRuntime
        do {
            signalRuntime = try TerminationSignalRuntime.install()
        } catch {
            safeFailure("signal setup failure")
            exit(1)
        }

        Task {
            exit(await run(arguments: CommandLine.arguments, signalRuntime: signalRuntime))
        }
        dispatchMain()
    }
}
