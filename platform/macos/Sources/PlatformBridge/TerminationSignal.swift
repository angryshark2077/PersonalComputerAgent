import CSignalRelay
import Darwin
import Foundation

enum TerminationSignalError: Error, Sendable {
    case relayInstallationFailed
}

actor TerminationSignalCoordinator {
    private var received = false
    private var server: BridgeServer?

    func register(server: BridgeServer) async throws -> Bool {
        if received {
            try await server.shutdown()
            return false
        }
        self.server = server
        return true
    }

    func receive(_ signal: Int32) async {
        guard signal == SIGINT || signal == SIGTERM, !received else { return }
        received = true
        if let server {
            try? await server.shutdown()
        }
    }
}

struct TerminationSignalRelay: Sendable {
    let readDescriptor: Int32

    static func install() throws -> TerminationSignalRelay {
        var descriptor: Int32 = -1
        guard pca_signal_relay_install(&descriptor) == 0, descriptor >= 0 else {
            throw TerminationSignalError.relayInstallationFailed
        }
        // The executable owns the installed handler and relay until Darwin.exit; avoiding teardown
        // prevents a handler from ever writing through a closed and subsequently reused descriptor.
        return TerminationSignalRelay(readDescriptor: descriptor)
    }

    func isSignaled() -> Bool {
        var descriptor = pollfd(
            fd: readDescriptor,
            events: Int16(POLLIN),
            revents: 0
        )
        while true {
            let result = Darwin.poll(&descriptor, 1, 0)
            if result >= 0 {
                return result > 0 && descriptor.revents & Int16(POLLIN) != 0
            }
            if errno != EINTR { return false }
        }
    }

    func startReader(coordinator: TerminationSignalCoordinator) {
        Thread.detachNewThread {
            guard let signal = waitForSignal() else { return }
            Task {
                await coordinator.receive(signal)
            }
        }
    }

    private func waitForSignal() -> Int32? {
        while true {
            var descriptor = pollfd(
                fd: readDescriptor,
                events: Int16(POLLIN),
                revents: 0
            )
            let pollResult = Darwin.poll(&descriptor, 1, -1)
            if pollResult < 0 {
                if errno == EINTR { continue }
                return nil
            }
            var markers = [UInt8](repeating: 0, count: 32)
            let count = Darwin.read(readDescriptor, &markers, markers.count)
            if count > 0 { return Int32(markers[0]) }
            if count < 0, errno == EINTR || errno == EAGAIN { continue }
            return nil
        }
    }
}

struct TerminationSignalRuntime: Sendable {
    let relay: TerminationSignalRelay
    let coordinator: TerminationSignalCoordinator

    static func install() throws -> TerminationSignalRuntime {
        TerminationSignalRuntime(
            relay: try TerminationSignalRelay.install(),
            coordinator: TerminationSignalCoordinator()
        )
    }

    func terminationAccepted() -> Bool {
        relay.isSignaled()
    }

    func startReader() {
        relay.startReader(coordinator: coordinator)
    }
}
