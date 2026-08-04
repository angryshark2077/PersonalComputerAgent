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

    var payload: [String: JSONValue] {
        [
            "interface_type": .string(interfaceType),
            "wifi_identity_available": .bool(wifiIdentityAvailable),
            "ssid": ssid.map(JSONValue.string) ?? .null,
            "bssid": bssid.map(JSONValue.string) ?? .null,
            "local_ipv4": localIPv4.map(JSONValue.string) ?? .null,
            "local_ipv6": localIPv6.map(JSONValue.string) ?? .null,
        ]
    }
}

final class NetworkObservationSource: @unchecked Sendable {
    private let monitor = NWPathMonitor()
    private let locationManager = CLLocationManager()
    private let queue = DispatchQueue(label: "com.pca.platform-bridge.network")
    private let lock = NSLock()
    private var latestPath: NWPath?

    init() {
        monitor.pathUpdateHandler = { [weak self] path in
            self?.lock.withLock { self?.latestPath = path }
        }
        monitor.start(queue: queue)
    }

    deinit {
        monitor.cancel()
    }

    func capture() -> NetworkObservation {
        let path = lock.withLock { latestPath } ?? monitor.currentPath
        let interfaceType = Self.interfaceType(path)
        let interfaceName = Self.interfaceName(path, type: interfaceType)
        let addresses = interfaceName.map(Self.addresses) ?? (nil, nil)
        let wifi = interfaceType == "wifi" && locationAccessGranted(locationManager.authorizationStatus)
            ? CWWiFiClient.shared().interface(withName: interfaceName)
            : nil
        let ssid = wifi?.ssid()?.precomposedStringWithCanonicalMapping
        let bssid = wifi?.bssid()?.uppercased()
        return NetworkObservation(
            interfaceType: interfaceType,
            wifiIdentityAvailable: interfaceType == "wifi" && ssid != nil && bssid != nil,
            ssid: ssid,
            bssid: bssid,
            localIPv4: addresses.0,
            localIPv6: addresses.1
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
