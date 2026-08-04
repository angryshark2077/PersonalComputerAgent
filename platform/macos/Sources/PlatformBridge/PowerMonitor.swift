import AppKit
import BridgeProtocol
import Foundation

enum PowerNotification: Sendable {
    case willSleep
    case didWake
}

enum PowerLifecycleEvent: String, Sendable {
    case systemSleep = "system.sleep"
    case systemWake = "system.wake"
}

enum PlatformLifecycleEventType: String, Sendable {
    case systemSleep = "system.sleep"
    case systemWake = "system.wake"
    case networkOffline = "network.offline"
    case networkOnline = "network.online"
}

struct PlatformLifecycleEvent: Sendable, Equatable {
    let sequence: UInt64
    let eventID: UUID
    let eventType: PlatformLifecycleEventType
    let occurredAt: String

    var payload: JSONValue {
        .object([
            "sequence": .number(Double(sequence)),
            "event_id": .string(eventID.uuidString.lowercased()),
            "event_type": .string(eventType.rawValue),
            "occurred_at": .string(occurredAt),
        ])
    }
}

final class PlatformLifecycleEventBuffer: @unchecked Sendable {
    private let lock = NSLock()
    private var nextSequence: UInt64 = 1
    private var events: [PlatformLifecycleEvent] = []
    private let capacity: Int

    init(capacity: Int = 64) {
        self.capacity = max(capacity, 1)
    }

    func record(_ eventType: PlatformLifecycleEventType, at date: Date = Date()) {
        lock.withLock {
            events.append(PlatformLifecycleEvent(
                sequence: nextSequence,
                eventID: UUID(),
                eventType: eventType,
                occurredAt: date.ISO8601Format()
            ))
            nextSequence = nextSequence &+ 1
            if events.count > capacity { events.removeFirst(events.count - capacity) }
        }
    }

    func snapshot(after sequence: UInt64) -> (events: [PlatformLifecycleEvent], latestSequence: UInt64) {
        lock.withLock {
            (events.filter { $0.sequence > sequence }, nextSequence &- 1)
        }
    }
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
