import Darwin
import Foundation

enum TerminationSignalError: Error, Sendable {
    case maskInstallationFailed
    case latchInstallationFailed
}

struct TerminationSignalLatch: Sendable {
    let readDescriptor: Int32
    let writeDescriptor: Int32

    static func make() throws -> TerminationSignalLatch {
        var descriptors = [Int32](repeating: -1, count: 2)
        guard pipe(&descriptors) == 0 else {
            throw TerminationSignalError.latchInstallationFailed
        }
        guard configure(descriptors[0]), configure(descriptors[1]) else {
            Darwin.close(descriptors[0])
            Darwin.close(descriptors[1])
            throw TerminationSignalError.latchInstallationFailed
        }
        return TerminationSignalLatch(
            readDescriptor: descriptors[0],
            writeDescriptor: descriptors[1]
        )
    }

    func markAccepted() {
        var marker: UInt8 = 1
        while true {
            let result = Darwin.write(writeDescriptor, &marker, 1)
            if result == 1 || (result == -1 && errno == EAGAIN) { return }
            if result != -1 || errno != EINTR { return }
        }
    }

    func isAccepted() -> Bool {
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

    private static func configure(_ descriptor: Int32) -> Bool {
        let descriptorFlags = fcntl(descriptor, F_GETFD)
        guard descriptorFlags >= 0,
              fcntl(descriptor, F_SETFD, descriptorFlags | FD_CLOEXEC) == 0 else {
            return false
        }
        let statusFlags = fcntl(descriptor, F_GETFL)
        return statusFlags >= 0
            && fcntl(descriptor, F_SETFL, statusFlags | O_NONBLOCK) == 0
    }
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

    static func start(
        latch: TerminationSignalLatch,
        coordinator: TerminationSignalCoordinator
    ) {
        Thread.detachNewThread {
            var waiterMask = signalMask()
            var receivedSignal: Int32 = 0
            guard sigwait(&waiterMask, &receivedSignal) == 0 else { return }
            latch.markAccepted()
            Task {
                await coordinator.receive(receivedSignal)
            }
        }
    }

    static func hasPendingTerminationSignal() -> Bool {
        var pending = sigset_t()
        guard sigpending(&pending) == 0 else { return false }
        return sigismember(&pending, SIGINT) == 1 || sigismember(&pending, SIGTERM) == 1
    }

    private static func signalMask() -> sigset_t {
        var mask = sigset_t()
        sigemptyset(&mask)
        sigaddset(&mask, SIGINT)
        sigaddset(&mask, SIGTERM)
        return mask
    }
}

struct TerminationSignalRuntime: Sendable {
    let latch: TerminationSignalLatch
    let coordinator: TerminationSignalCoordinator

    static func install() throws -> TerminationSignalRuntime {
        try TerminationSignalWaiter.block()
        let latch = try TerminationSignalLatch.make()
        let coordinator = TerminationSignalCoordinator()
        TerminationSignalWaiter.start(latch: latch, coordinator: coordinator)
        return TerminationSignalRuntime(latch: latch, coordinator: coordinator)
    }

    func terminationAcceptedOrPending() -> Bool {
        if TerminationSignalWaiter.hasPendingTerminationSignal() { return true }
        if latch.isAccepted() { return true }
        if TerminationSignalWaiter.hasPendingTerminationSignal() { return true }
        return latch.isAccepted()
    }
}
