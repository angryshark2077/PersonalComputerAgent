import AppKit
import Foundation

enum PowerNotification: Sendable {
    case willSleep
    case didWake
}

enum PowerLifecycleEvent: String, Sendable {
    case systemSleep = "SYSTEM_SLEEP"
    case systemWake = "SYSTEM_WAKE"
}

@MainActor
final class PowerMonitor {
    private let handler: @Sendable (PowerLifecycleEvent) -> Void
    private var observers: [NSObjectProtocol] = []

    init(handler: @escaping @Sendable (PowerLifecycleEvent) -> Void) {
        self.handler = handler
    }

    func start() {
        guard observers.isEmpty else { return }
        let center = NSWorkspace.shared.notificationCenter
        observers.append(center.addObserver(
            forName: NSWorkspace.willSleepNotification,
            object: nil,
            queue: .main
        ) { [handler] _ in
            handler(.systemSleep)
        })
        observers.append(center.addObserver(
            forName: NSWorkspace.didWakeNotification,
            object: nil,
            queue: .main
        ) { [handler] _ in
            handler(.systemWake)
        })
    }

    func stop() {
        let center = NSWorkspace.shared.notificationCenter
        observers.forEach(center.removeObserver)
        observers.removeAll()
    }

    nonisolated static func map(_ notification: PowerNotification) -> PowerLifecycleEvent {
        switch notification {
        case .willSleep: .systemSleep
        case .didWake: .systemWake
        }
    }
}
