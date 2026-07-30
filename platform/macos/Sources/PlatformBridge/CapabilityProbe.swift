import CoreGraphics
import Foundation

enum PermissionStatus: String, Codable, CaseIterable, Sendable {
    case notDetermined = "not_determined"
    case granted
    case denied
    case restricted
    case unavailable
}

enum RawPermissionStatus: Sendable {
    case notDetermined
    case granted
    case denied
    case restricted
    case unavailable
}

protocol CapabilityStatusSource: Sendable {
    func screenCaptureStatus() -> RawPermissionStatus
}

struct SystemCapabilityStatusSource: CapabilityStatusSource {
    func screenCaptureStatus() -> RawPermissionStatus {
        CGPreflightScreenCaptureAccess() ? .granted : .denied
    }
}

struct CapabilityProbe: Sendable {
    let source: any CapabilityStatusSource

    init(source: any CapabilityStatusSource = SystemCapabilityStatusSource()) {
        self.source = source
    }

    func screenCapturePermission() -> PermissionStatus {
        Self.map(source.screenCaptureStatus())
    }

    static func map(_ raw: RawPermissionStatus) -> PermissionStatus {
        switch raw {
        case .notDetermined: .notDetermined
        case .granted: .granted
        case .denied: .denied
        case .restricted: .restricted
        case .unavailable: .unavailable
        }
    }
}
