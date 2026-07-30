import Foundation
@testable import PlatformBridge
import XCTest

final class CapabilityProbeTests: XCTestCase {
    func testEveryRawTCCStatusMapsToCanonicalPermissionStatus() {
        let cases: [(RawPermissionStatus, PermissionStatus)] = [
            (.notDetermined, .notDetermined),
            (.granted, .granted),
            (.denied, .denied),
            (.restricted, .restricted),
            (.unavailable, .unavailable),
        ]

        for (raw, expected) in cases {
            XCTAssertEqual(CapabilityProbe.map(raw), expected)
            XCTAssertEqual(expected.rawValue, raw.expectedWireValue)
        }
    }

    func testProbeReadsInjectedStatusWithoutPrompting() {
        let source = FixedCapabilityStatusSource(status: .restricted)
        let probe = CapabilityProbe(source: source)

        XCTAssertEqual(probe.screenCapturePermission(), .restricted)
    }

    func testSyntheticSleepAndWakeMapToCanonicalLifecycleEvents() {
        XCTAssertEqual(PowerMonitor.map(.willSleep), .systemSleep)
        XCTAssertEqual(PowerMonitor.map(.didWake), .systemWake)
        XCTAssertEqual(PowerLifecycleEvent.systemSleep.rawValue, "SYSTEM_SLEEP")
        XCTAssertEqual(PowerLifecycleEvent.systemWake.rawValue, "SYSTEM_WAKE")
    }
}

private struct FixedCapabilityStatusSource: CapabilityStatusSource {
    let status: RawPermissionStatus

    func screenCaptureStatus() -> RawPermissionStatus {
        status
    }
}

private extension RawPermissionStatus {
    var expectedWireValue: String {
        switch self {
        case .notDetermined: "not_determined"
        case .granted: "granted"
        case .denied: "denied"
        case .restricted: "restricted"
        case .unavailable: "unavailable"
        }
    }
}
