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

do {
    let arguments = try BridgeArguments.parse(CommandLine.arguments)
    let validator = SocketPathValidator()
    try validator.validate(socketURL: arguments.socketURL)

    let server = BridgeServer(
        socketURL: arguments.socketURL,
        pathValidator: validator,
        handshakeHandler: HandshakeHandler(
            credentialProvider: KeychainBridgeCredentialProvider(),
            bridgeVersion: "0.0.0-s1a"
        )
    )
    Darwin.signal(SIGINT, SIG_IGN)
    Darwin.signal(SIGTERM, SIG_IGN)
    let interruptSource = DispatchSource.makeSignalSource(signal: SIGINT)
    let terminateSource = DispatchSource.makeSignalSource(signal: SIGTERM)
    interruptSource.setEventHandler {
        Task { await server.shutdown() }
    }
    terminateSource.setEventHandler {
        Task { await server.shutdown() }
    }
    interruptSource.resume()
    terminateSource.resume()
    defer {
        interruptSource.cancel()
        terminateSource.cancel()
    }

    try await server.start()

    let powerMonitor = await MainActor.run {
        let monitor = PowerMonitor { _ in
            // The typed callback is intentionally not serialized until the lifecycle wire schema is frozen.
        }
        monitor.start()
        return monitor
    }
    do {
        try await server.serve()
    } catch {
        await server.shutdown()
        await MainActor.run { powerMonitor.stop() }
        throw error
    }
    await server.shutdown()
    await MainActor.run { powerMonitor.stop() }
} catch {
    safeFailure("startup or server failure")
    exit(1)
}
