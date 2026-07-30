import Darwin
import Foundation

enum TerminationSignalError: Error, Sendable {
    case maskInstallationFailed
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

enum TerminationSignalWaiter {
    static func block() throws {
        var mask = signalMask()
        guard pthread_sigmask(SIG_BLOCK, &mask, nil) == 0 else {
            throw TerminationSignalError.maskInstallationFailed
        }
    }

    static func start(coordinator: TerminationSignalCoordinator) {
        Thread.detachNewThread {
            var waiterMask = signalMask()
            var receivedSignal: Int32 = 0
            guard sigwait(&waiterMask, &receivedSignal) == 0 else { return }
            Task {
                await coordinator.receive(receivedSignal)
            }
        }
    }

    private static func signalMask() -> sigset_t {
        var mask = sigset_t()
        sigemptyset(&mask)
        sigaddset(&mask, SIGINT)
        sigaddset(&mask, SIGTERM)
        return mask
    }
}
