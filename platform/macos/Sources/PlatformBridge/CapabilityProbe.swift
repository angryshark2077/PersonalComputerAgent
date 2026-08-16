import ApplicationServices
import AVFoundation
import CoreGraphics
import Foundation

enum PermissionStatus: String, Codable, CaseIterable, Sendable {
    case notDetermined = "not_determined"
    case granted
    case denied
    case restricted
    case unavailable
}

enum RawPermissionStatus: CaseIterable, Equatable, Sendable {
    case notDetermined
    case granted
    case denied
    case restricted
    case unavailable
}

enum PlatformCapability: CaseIterable, Hashable, Sendable {
    case screenCapture
    case accessibility
    case camera
    case microphone
}

protocol CapabilityStatusSource: Sendable {
    func status(for capability: PlatformCapability) -> RawPermissionStatus
}

struct SystemCapabilityStatusSource: CapabilityStatusSource {
    func status(for capability: PlatformCapability) -> RawPermissionStatus {
        switch capability {
        case .screenCapture:
            CGPreflightScreenCaptureAccess() ? .granted : .denied
        case .accessibility:
            AXIsProcessTrusted() ? .granted : .denied
        case .camera:
            Self.map(AVCaptureDevice.authorizationStatus(for: .video))
        case .microphone:
            Self.map(AVCaptureDevice.authorizationStatus(for: .audio))
        }
    }

    static func map(_ status: AVAuthorizationStatus) -> RawPermissionStatus {
        switch status {
        case .notDetermined: .notDetermined
        case .authorized: .granted
        case .denied: .denied
        case .restricted: .restricted
        @unknown default: .unavailable
        }
    }
}

struct CapabilityProbe: Sendable {
    let source: any CapabilityStatusSource

    init(source: any CapabilityStatusSource = SystemCapabilityStatusSource()) {
        self.source = source
    }

    func screenCapturePermission() -> PermissionStatus {
        permission(for: .screenCapture)
    }

    func screenCaptureAvailability() -> String {
        switch screenCapturePermission() {
        case .granted: "available"
        case .notDetermined: "not_determined"
        case .denied, .restricted: "permission_required"
        case .unavailable: "unavailable"
        }
    }

    func permissionSnapshot() -> [String: PermissionStatus] {
        Dictionary(uniqueKeysWithValues: PlatformCapability.allCases.map { capability in
            (capability.wireKey, permission(for: capability))
        })
    }

    func permission(for capability: PlatformCapability) -> PermissionStatus {
        Self.map(source.status(for: capability))
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

private extension PlatformCapability {
    var wireKey: String {
        switch self {
        case .screenCapture: "screen_capture"
        case .accessibility: "accessibility"
        case .camera: "camera"
        case .microphone: "microphone"
        }
    }
}
