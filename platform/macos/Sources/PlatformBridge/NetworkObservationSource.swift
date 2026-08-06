import BridgeProtocol
import CoreLocation
import CoreWLAN
import Darwin
import Foundation
import Network

func locationAccessGranted(_ status: CLAuthorizationStatus) -> Bool {
    status == .authorizedAlways
}

struct NetworkObservation: Equatable, Sendable {
    let interfaceType: String
    let wifiIdentityAvailable: Bool
    let ssid: String?
    let bssid: String?
    let localIPv4: String?
    let localIPv6: String?
    let location: DeviceLocationObservation?

    var payload: [String: JSONValue] {
        [
            "interface_type": .string(interfaceType),
            "wifi_identity_available": .bool(wifiIdentityAvailable),
            "ssid": ssid.map(JSONValue.string) ?? .null,
            "bssid": bssid.map(JSONValue.string) ?? .null,
            "local_ipv4": localIPv4.map(JSONValue.string) ?? .null,
            "local_ipv6": localIPv6.map(JSONValue.string) ?? .null,
            "location": location?.payload ?? .null,
        ]
    }
}

enum NetworkReachabilityState: Equatable, Sendable {
    case offline
    case online
}

struct NetworkPathIdentity: Equatable, Sendable {
    let interfaceName: String?
    let ssid: String?
    let bssid: String?
    let localIPv4: String?
    let localIPv6: String?
}

func networkLifecycleTransition(
    previousState: NetworkReachabilityState?,
    currentState: NetworkReachabilityState,
    previousIdentity: NetworkPathIdentity?,
    currentIdentity: NetworkPathIdentity?
) -> PlatformLifecycleEventType? {
    guard let previousState else { return nil }
    if previousState == .online, currentState == .offline { return .networkOffline }
    if previousState == .offline, currentState == .online { return .networkOnline }
    if currentState == .online,
       let previousIdentity,
       let currentIdentity,
       previousIdentity != currentIdentity { return .networkChanged }
    return nil
}

struct DeviceLocationObservation: Equatable, Sendable {
    let latitude: Double
    let longitude: Double
    let horizontalAccuracyMeters: Double
    let observedAt: String

    var payload: JSONValue {
        .object([
            "latitude": .number(latitude),
            "longitude": .number(longitude),
            "horizontal_accuracy_meters": .number(horizontalAccuracyMeters),
            "observed_at": .string(observedAt),
        ])
    }
}

func validDeviceLocation(
    latitude: Double,
    longitude: Double,
    horizontalAccuracyMeters: Double
) -> Bool {
    latitude.isFinite && (-90 ... 90).contains(latitude)
        && longitude.isFinite && (-180 ... 180).contains(longitude)
        && horizontalAccuracyMeters.isFinite
        && (0 ... 100_000).contains(horizontalAccuracyMeters)
}

func normalizedWiFiSSID(_ value: String?) -> String? {
    guard let value else { return nil }
    let normalized = value.precomposedStringWithCanonicalMapping
    guard !normalized.isEmpty, normalized.utf8.count <= 128 else { return nil }
    return normalized
}

func normalizedWiFiBSSID(_ value: String?) -> String? {
    guard let value else { return nil }
    let normalized = value.uppercased()
    let octets = normalized.split(separator: ":", omittingEmptySubsequences: false)
    guard octets.count == 6,
          octets.allSatisfy({ octet in
              octet.count == 2 && octet.utf8.allSatisfy { byte in
                  (48 ... 57).contains(byte) || (65 ... 70).contains(byte)
              }
          }) else { return nil }
    return normalized
}

final class DeviceLocationSource: NSObject, CLLocationManagerDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var manager: CLLocationManager?
    private var latest: DeviceLocationObservation?
    private var latestObservedAt: Date?
    private var requestInFlight = false
    private var lastRequestAt: Date?

    override init() {
        super.init()
        Task { @MainActor [weak self] in
            self?.start()
        }
    }

    func current() -> DeviceLocationObservation? {
        refreshIfNeeded()
        return lock.withLock {
            guard let latestObservedAt,
                  Date().timeIntervalSince(latestObservedAt) <= 600 else { return nil }
            return latest
        }
    }

    @MainActor
    private func start() {
        let manager = CLLocationManager()
        manager.desiredAccuracy = kCLLocationAccuracyBest
        self.manager = manager
        manager.delegate = self
    }

    private func refreshIfNeeded() {
        let shouldRequest = lock.withLock {
            !requestInFlight && (lastRequestAt.map { Date().timeIntervalSince($0) >= 300 } ?? true)
        }
        guard shouldRequest else { return }
        Task { @MainActor [weak self] in
            self?.requestLocationIfAuthorized()
        }
    }

    @MainActor
    private func requestLocationIfAuthorized() {
        guard let manager,
              locationAccessGranted(manager.authorizationStatus) else { return }
        let shouldRequest = lock.withLock {
            guard !requestInFlight else { return false }
            requestInFlight = true
            lastRequestAt = Date()
            return true
        }
        if shouldRequest { manager.requestLocation() }
    }

    func locationManager(_ manager: CLLocationManager, didUpdateLocations locations: [CLLocation]) {
        let location = locations
            .filter {
                validDeviceLocation(
                    latitude: $0.coordinate.latitude,
                    longitude: $0.coordinate.longitude,
                    horizontalAccuracyMeters: $0.horizontalAccuracy
                )
            }
            .max(by: { $0.timestamp < $1.timestamp })
        let observation = location.map {
            DeviceLocationObservation(
                latitude: $0.coordinate.latitude,
                longitude: $0.coordinate.longitude,
                horizontalAccuracyMeters: $0.horizontalAccuracy,
                observedAt: $0.timestamp.ISO8601Format()
            )
        }
        lock.withLock {
            if let observation, let location {
                latest = observation
                latestObservedAt = location.timestamp
            }
            requestInFlight = false
        }
    }

    func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        if !locationAccessGranted(manager.authorizationStatus) {
            lock.withLock {
                latest = nil
                latestObservedAt = nil
                requestInFlight = false
            }
        }
    }

    func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {
        lock.withLock { requestInFlight = false }
    }
}

final class NetworkObservationSource: @unchecked Sendable {
    private let monitor = NWPathMonitor()
    private let wifiMonitor = NWPathMonitor(requiredInterfaceType: .wifi)
    private let locationManager = CLLocationManager()
    private let deviceLocationSource = DeviceLocationSource()
    private let queue = DispatchQueue(label: "com.pca.platform-bridge.network")
    private let lock = NSLock()
    private let lifecycleSource: PlatformLifecycleEventBuffer
    private var latestPath: NWPath?
    private var latestWiFiState: NetworkReachabilityState?
    private var latestWiFiIdentity: NetworkPathIdentity?

    init(lifecycleSource: PlatformLifecycleEventBuffer = PlatformLifecycleEventBuffer()) {
        self.lifecycleSource = lifecycleSource
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            lock.withLock { latestPath = path }
        }
        monitor.start(queue: queue)
        wifiMonitor.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            let currentState: NetworkReachabilityState = path.status == .satisfied ? .online : .offline
            let currentIdentity = currentState == .online ? networkPathIdentity(path) : nil
            let transition = lock.withLock { () -> PlatformLifecycleEventType? in
                let transition = networkLifecycleTransition(
                    previousState: latestWiFiState,
                    currentState: currentState,
                    previousIdentity: latestWiFiIdentity,
                    currentIdentity: currentIdentity
                )
                latestWiFiState = currentState
                latestWiFiIdentity = currentIdentity
                return transition
            }
            if let transition { lifecycleSource.record(transition) }
        }
        wifiMonitor.start(queue: queue)
    }

    deinit {
        monitor.cancel()
        wifiMonitor.cancel()
    }

    func capture() -> NetworkObservation {
        let path = lock.withLock { latestPath } ?? monitor.currentPath
        let interfaceType = Self.interfaceType(path)
        let interfaceName = Self.interfaceName(path, type: interfaceType)
        let addresses = interfaceName.map(Self.addresses) ?? (nil, nil)
        let wifi = interfaceType == "wifi" && locationAccessGranted(locationManager.authorizationStatus)
            ? CWWiFiClient.shared().interface()
            : nil
        let ssid = normalizedWiFiSSID(wifi?.ssid())
        let bssid = normalizedWiFiBSSID(wifi?.bssid())
        return NetworkObservation(
            interfaceType: interfaceType,
            wifiIdentityAvailable: interfaceType == "wifi" && ssid != nil && bssid != nil,
            ssid: ssid,
            bssid: bssid,
            localIPv4: addresses.0,
            localIPv6: addresses.1,
            location: deviceLocationSource.current()
        )
    }

    private static func interfaceType(_ path: NWPath?) -> String {
        guard let path, path.status == .satisfied else { return "none" }
        if path.usesInterfaceType(.wifi) { return "wifi" }
        if path.usesInterfaceType(.wiredEthernet) { return "wired" }
        return "other"
    }

    private static func interfaceName(_ path: NWPath?, type: String) -> String? {
        guard let path else { return nil }
        return path.availableInterfaces.first { interface in
            switch type {
            case "wifi": interface.type == .wifi
            case "wired": interface.type == .wiredEthernet
            case "other": path.usesInterfaceType(interface.type)
            default: false
            }
        }?.name
    }

    private func networkPathIdentity(_ path: NWPath) -> NetworkPathIdentity {
        let interfaceName = Self.interfaceName(path, type: "wifi")
        let addresses = interfaceName.map(Self.addresses) ?? (nil, nil)
        let wifi = locationAccessGranted(locationManager.authorizationStatus)
            ? CWWiFiClient.shared().interface()
            : nil
        return NetworkPathIdentity(
            interfaceName: interfaceName,
            ssid: normalizedWiFiSSID(wifi?.ssid()),
            bssid: normalizedWiFiBSSID(wifi?.bssid()),
            localIPv4: addresses.0,
            localIPv6: addresses.1
        )
    }

    private static func addresses(_ interfaceName: String) -> (String?, String?) {
        var first: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&first) == 0, let first else { return (nil, nil) }
        defer { freeifaddrs(first) }
        var ipv4: String?
        var ipv6: String?
        var current: UnsafeMutablePointer<ifaddrs>? = first
        while let address = current?.pointee {
            defer { current = address.ifa_next }
            guard String(cString: address.ifa_name) == interfaceName,
                  let socketAddress = address.ifa_addr else { continue }
            let family = Int32(socketAddress.pointee.sa_family)
            guard family == AF_INET || family == AF_INET6 else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            let length = family == AF_INET
                ? socklen_t(MemoryLayout<sockaddr_in>.size)
                : socklen_t(MemoryLayout<sockaddr_in6>.size)
            guard getnameinfo(
                socketAddress,
                length,
                &host,
                socklen_t(host.count),
                nil,
                0,
                NI_NUMERICHOST
            ) == 0 else { continue }
            let value = String(decoding: host.prefix { $0 != 0 }.map(UInt8.init(bitPattern:)), as: UTF8.self)
            if family == AF_INET,
               !value.hasPrefix("127."),
               !value.hasPrefix("169.254.") { ipv4 = ipv4 ?? value }
            if family == AF_INET6,
               value != "::1",
               !value.lowercased().hasPrefix("fe80:") { ipv6 = ipv6 ?? value }
        }
        return (ipv4, ipv6)
    }
}
