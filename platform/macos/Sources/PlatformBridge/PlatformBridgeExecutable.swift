import AppKit
import CoreLocation
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
    private static let locationAuthorizationArgument = "--authorize-location"
    private static let screenCaptureAuthorizationArgument = "--authorize-screen-capture"

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
            let monitor = PowerMonitor { event in
                server.recordPowerLifecycleEvent(event)
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

    @MainActor
    public static func main() -> Never {
        if CommandLine.arguments == [CommandLine.arguments[0], locationAuthorizationArgument] {
            Task { @MainActor in
                exit(await requestLocationAuthorization())
            }
            NSApplication.shared.run()
            exit(3)
        }
        if CommandLine.arguments == [CommandLine.arguments[0], screenCaptureAuthorizationArgument] {
            NSApplication.shared.setActivationPolicy(.accessory)
            NSApplication.shared.activate(ignoringOtherApps: true)
            exit(CGPreflightScreenCaptureAccess() || CGRequestScreenCaptureAccess() ? 0 : 2)
        }

        NSApplication.shared.setActivationPolicy(.prohibited)

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
        NSApplication.shared.run()
        exit(3)
    }

    @MainActor
    private static func requestLocationAuthorization() async -> Int32 {
        let manager = CLLocationManager()
        if locationAccessGranted(manager.authorizationStatus) { return 0 }
        if manager.authorizationStatus == .denied || manager.authorizationStatus == .restricted { return 2 }

        NSApplication.shared.setActivationPolicy(.accessory)
        let activationWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1, height: 1),
            styleMask: .borderless,
            backing: .buffered,
            defer: false
        )
        activationWindow.alphaValue = 0
        activationWindow.makeKeyAndOrderFront(nil)
        defer { activationWindow.close() }
        NSApplication.shared.activate(ignoringOtherApps: true)
        let clock = ContinuousClock()
        let activationDeadline = clock.now.advanced(by: .seconds(2))
        while !NSApplication.shared.isActive, clock.now < activationDeadline {
            try? await Task.sleep(for: .milliseconds(100))
        }
        manager.requestWhenInUseAuthorization()
        manager.startUpdatingLocation()
        defer { manager.stopUpdatingLocation() }

        let deadline = clock.now.advanced(by: .seconds(300))
        while clock.now < deadline {
            if locationAccessGranted(manager.authorizationStatus) { return 0 }
            if manager.authorizationStatus == .denied || manager.authorizationStatus == .restricted { return 2 }
            try? await Task.sleep(for: .milliseconds(250))
        }
        return 3
    }
}
