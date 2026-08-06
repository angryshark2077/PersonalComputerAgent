import AVFoundation
import BridgeProtocol
import CoreLocation
import Foundation
@testable import PlatformBridge
import XCTest

final class CapabilityProbeTests: XCTestCase {
    func testDeviceLocationObservationUsesStrictCoordinatePayload() {
        let observation = DeviceLocationObservation(
            latitude: 1.352083,
            longitude: 103.819836,
            horizontalAccuracyMeters: 24.5,
            observedAt: "2026-08-04T09:00:00Z"
        )

        XCTAssertEqual(observation.payload, .object([
            "latitude": .number(1.352083),
            "longitude": .number(103.819836),
            "horizontal_accuracy_meters": .number(24.5),
            "observed_at": .string("2026-08-04T09:00:00Z"),
        ]))
    }

    func testDeviceLocationValidationRejectsValuesThatCannotCrossTheBridgeContract() {
        XCTAssertTrue(validDeviceLocation(
            latitude: 31.2931614,
            longitude: 121.3167298,
            horizontalAccuracyMeters: 24.5
        ))
        XCTAssertFalse(validDeviceLocation(
            latitude: .infinity,
            longitude: 121.3167298,
            horizontalAccuracyMeters: 24.5
        ))
        XCTAssertFalse(validDeviceLocation(
            latitude: 31.2931614,
            longitude: 181,
            horizontalAccuracyMeters: 24.5
        ))
        XCTAssertFalse(validDeviceLocation(
            latitude: 31.2931614,
            longitude: 121.3167298,
            horizontalAccuracyMeters: 100_001
        ))
    }

    func testWiFiIdentityNormalizationRejectsValuesThatWouldInvalidateObservation() {
        XCTAssertEqual(normalizedWiFiSSID("ChinaNet-G4NS-5G"), "ChinaNet-G4NS-5G")
        XCTAssertNil(normalizedWiFiSSID(""))
        XCTAssertNil(normalizedWiFiSSID(String(repeating: "网", count: 43)))

        XCTAssertEqual(normalizedWiFiBSSID("aa:bb:0c:01:02:ff"), "AA:BB:0C:01:02:FF")
        XCTAssertNil(normalizedWiFiBSSID("AA:BB:CC:DD:EE"))
        XCTAssertNil(normalizedWiFiBSSID("AA:BB:CC:DD:EE:GG"))
        XCTAssertNil(normalizedWiFiBSSID("0:0:0:0:0:0"))
    }

    func testWiFiLocationGateAcceptsOnlyTheGrantedMacOSCoreLocationStatus() {
        XCTAssertTrue(locationAccessGranted(.authorizedAlways))
        XCTAssertFalse(locationAccessGranted(.notDetermined))
        XCTAssertFalse(locationAccessGranted(.denied))
        XCTAssertFalse(locationAccessGranted(.restricted))
    }

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
        let source = FixedCapabilityStatusSource(statuses: [.screenCapture: .restricted])
        let probe = CapabilityProbe(source: source)

        XCTAssertEqual(probe.screenCapturePermission(), .restricted)
    }

    func testAVFoundationAuthorizationStatusesMapWithoutRequestingAccess() {
        let cases: [(AVAuthorizationStatus, RawPermissionStatus)] = [
            (.notDetermined, .notDetermined),
            (.authorized, .granted),
            (.denied, .denied),
            (.restricted, .restricted),
        ]

        for (status, expected) in cases {
            XCTAssertEqual(SystemCapabilityStatusSource.map(status), expected)
        }
    }

    func testProductionReadOnlySourceReturnsCanonicalStatusForEverySupportedCapability() {
        let source = SystemCapabilityStatusSource()

        for capability in PlatformCapability.allCases {
            XCTAssertTrue(RawPermissionStatus.allCases.contains(source.status(for: capability)))
        }
    }

    func testSyntheticSleepAndWakeMapToCanonicalLifecycleEvents() {
        XCTAssertEqual(PowerMonitor.map(.willSleep), .systemSleep)
        XCTAssertEqual(PowerMonitor.map(.didWake), .systemWake)
        XCTAssertEqual(PowerLifecycleEvent.systemSleep.rawValue, "system.sleep")
        XCTAssertEqual(PowerLifecycleEvent.systemWake.rawValue, "system.wake")
    }

    func testLifecycleBufferReturnsStableEventsAfterTheRequestedSequence() {
        let source = PlatformLifecycleEventBuffer(capacity: 4)
        source.record(.systemSleep, at: Date(timeIntervalSince1970: 1))
        source.record(.systemWake, at: Date(timeIntervalSince1970: 2))

        let first = source.snapshot(after: 0)
        XCTAssertEqual(first.events.map(\.eventType), [.systemSleep, .systemWake])
        XCTAssertEqual(first.latestSequence, 2)
        XCTAssertEqual(source.snapshot(after: 1).events.map(\.eventType), [.systemWake])
    }

    func testWiFiLifecycleTracksPhysicalReachabilityEvenWhenAnotherRouteRemainsOnline() {
        let identity = NetworkPathIdentity(
            interfaceName: "en0",
            ssid: "Home",
            bssid: "AA:BB:CC:DD:EE:FF",
            localIPv4: "192.168.1.2",
            localIPv6: nil
        )

        XCTAssertEqual(networkLifecycleTransition(
            previousState: .online,
            currentState: .offline,
            previousIdentity: identity,
            currentIdentity: nil
        ), .networkOffline)
        XCTAssertEqual(networkLifecycleTransition(
            previousState: .offline,
            currentState: .online,
            previousIdentity: nil,
            currentIdentity: identity
        ), .networkOnline)
    }

    func testWiFiLifecycleReportsIdentityChangesWithoutAnOfflineGap() {
        let home = NetworkPathIdentity(
            interfaceName: "en0",
            ssid: "Home",
            bssid: "AA:BB:CC:DD:EE:FF",
            localIPv4: "192.168.1.2",
            localIPv6: nil
        )
        let hotspot = NetworkPathIdentity(
            interfaceName: "en0",
            ssid: "Jacob Hotspot",
            bssid: "11:22:33:44:55:66",
            localIPv4: "172.20.10.2",
            localIPv6: nil
        )

        XCTAssertEqual(networkLifecycleTransition(
            previousState: .online,
            currentState: .online,
            previousIdentity: home,
            currentIdentity: hotspot
        ), .networkChanged)
        XCTAssertNil(networkLifecycleTransition(
            previousState: .online,
            currentState: .online,
            previousIdentity: hotspot,
            currentIdentity: hotspot
        ))
    }
}

private struct FixedCapabilityStatusSource: CapabilityStatusSource {
    let statuses: [PlatformCapability: RawPermissionStatus]

    func status(for capability: PlatformCapability) -> RawPermissionStatus {
        statuses[capability] ?? .unavailable
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
