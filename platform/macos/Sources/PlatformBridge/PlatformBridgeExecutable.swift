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
        let server: BridgeServer
        do {
            let arguments = try BridgeArguments.parse(commandLineArguments)
            let validator = SocketPathValidator()
            try validator.validate(socketURL: arguments.socketURL)
            server = BridgeServer(
                socketURL: arguments.socketURL,
                pathValidator: validator,
                handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
                credentialProvider: KeychainBridgeCredentialProvider()
            )
        } catch {
            return startupFailureCode(signalRuntime: signalRuntime)
        }
        return await run(server: server, signalRuntime: signalRuntime)
    }

    static func run(
        server: BridgeServer,
        signalRuntime: TerminationSignalRuntime
    ) async -> Int32 {
        if let failureCode = await start(server: server, signalRuntime: signalRuntime) {
            return failureCode
        }
        signalRuntime.startReader()
        return await serveStarted(server: server)
    }

    static func start(
        server: BridgeServer,
        signalRuntime: TerminationSignalRuntime
    ) async -> Int32? {
        do {
            guard try await signalRuntime.coordinator.register(server: server) else { return 0 }
            try await server.start()
            return nil
        } catch BridgeServerError.shutdownRequested {
            return 0
        } catch {
            return startupFailureCode(signalRuntime: signalRuntime)
        }
    }

    static func serveStarted(server: BridgeServer) async -> Int32 {
        let powerMonitor = await MainActor.run {
            let monitor = PowerMonitor { _ in
                // The typed callback is intentionally not serialized until the lifecycle wire schema is frozen.
            }
            monitor.start()
            return monitor
        }
        do {
            try await server.serve()
            try await server.shutdown()
            await MainActor.run { powerMonitor.stop() }
            return 0
        } catch {
            do {
                try await server.shutdown()
            } catch {
                // The original runtime failure remains the selected result.
            }
            await MainActor.run { powerMonitor.stop() }
            safeFailure("server runtime failure")
            return 1
        }
    }

    private static func startupFailureCode(
        signalRuntime: TerminationSignalRuntime
    ) -> Int32 {
        if signalRuntime.terminationAccepted() { return 0 }
        safeFailure("startup failure")
        return 1
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
