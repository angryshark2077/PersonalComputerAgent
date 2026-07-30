import AppKit
import Darwin
import Foundation
import ServiceManagement

@MainActor
protocol ServiceBackend: AnyObject {
    func status() -> ServiceState
    func register() throws
    func unregister() async throws
    func openSystemSettingsLoginItems()
}

@MainActor
private final class SMServiceBackend: ServiceBackend {
    private let service: SMAppService

    init(plistName: String) { service = SMAppService.agent(plistName: plistName) }

    func status() -> ServiceState {
        switch service.status {
        case .notRegistered: .notRegistered
        case .enabled: .enabled
        case .requiresApproval: .requiresApproval
        case .notFound: .notFound
        @unknown default: .notFound
        }
    }

    func register() throws { try service.register() }
    func unregister() async throws { try await service.unregister() }
    func openSystemSettingsLoginItems() { SMAppService.openSystemSettingsLoginItems() }
}

@MainActor
final class ServiceController: ServiceControlling {
    private let backend: any ServiceBackend
    private let approvalTimeout: Duration
    private let pollInterval: Duration

    convenience init(
        plistName: String = BundleValidator.launchAgentName,
        approvalTimeout: Duration = .seconds(120),
        pollInterval: Duration = .milliseconds(500)
    ) {
        self.init(
            backend: SMServiceBackend(plistName: plistName),
            approvalTimeout: approvalTimeout,
            pollInterval: pollInterval
        )
    }

    init(backend: any ServiceBackend, approvalTimeout: Duration, pollInterval: Duration) {
        self.backend = backend
        self.approvalTimeout = approvalTimeout
        self.pollInterval = pollInterval
    }

    func status() -> ServiceState { backend.status() }

    func stopAndUnregister() async throws {
        switch backend.status() {
        case .notRegistered, .notFound:
            return
        case .enabled, .requiresApproval:
            do { try await backend.unregister() }
            catch { throw InstallError.serviceRegistrationFailed }
        }
    }

    func registerAndWaitForApproval(
        onWaitingForApproval: @escaping @MainActor () -> Void
    ) async throws {
        if backend.status() == .enabled { return }
        if backend.status() == .notRegistered || backend.status() == .notFound {
            do { try backend.register() }
            catch {
                if backend.status() != .requiresApproval { throw InstallError.serviceRegistrationFailed }
            }
        }
        guard backend.status() == .requiresApproval else {
            guard backend.status() == .enabled else { throw InstallError.serviceRegistrationFailed }
            return
        }

        onWaitingForApproval()
        backend.openSystemSettingsLoginItems()
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: approvalTimeout)
        while clock.now < deadline {
            try Task.checkCancellation()
            if backend.status() == .enabled { return }
            try await Task.sleep(for: pollInterval)
        }
        throw InstallError.approvalTimedOut
    }
}

@MainActor
protocol ProcessInspecting: AnyObject {
    func inspect(processID: Int32) -> ProcessIdentity?
}

struct ProcessIdentity: Equatable, Sendable {
    let userID: UInt32
    let executableURL: URL
    let startedAt: Date
}

@MainActor
private final class DarwinProcessInspector: ProcessInspecting {
    func inspect(processID: Int32) -> ProcessIdentity? {
        var info = proc_bsdinfo()
        let infoSize = Int32(MemoryLayout<proc_bsdinfo>.size)
        guard proc_pidinfo(processID, PROC_PIDTBSDINFO, 0, &info, infoSize) == infoSize else {
            return nil
        }

        var path = [CChar](repeating: 0, count: 4 * Int(MAXPATHLEN))
        guard proc_pidpath(processID, &path, UInt32(path.count)) > 0 else { return nil }
        let start = TimeInterval(info.pbi_start_tvsec)
            + TimeInterval(info.pbi_start_tvusec) / 1_000_000
        return ProcessIdentity(
            userID: info.pbi_uid,
            executableURL: URL(fileURLWithPath: String(cString: path)).standardizedFileURL,
            startedAt: Date(timeIntervalSince1970: start)
        )
    }
}

@MainActor
final class RuntimeHealthChecker: HealthChecking {
    private let pollInterval: Duration
    private let processInspector: any ProcessInspecting
    private static let fractionalRFC3339 = Date.ISO8601FormatStyle(includingFractionalSeconds: true)
    private static let wholeSecondRFC3339 = Date.ISO8601FormatStyle(includingFractionalSeconds: false)

    convenience init(
        pollInterval: Duration = .milliseconds(250)
    ) {
        self.init(pollInterval: pollInterval, processInspector: DarwinProcessInspector())
    }

    init(
        pollInterval: Duration = .milliseconds(250),
        processInspector: any ProcessInspecting
    ) {
        self.pollInterval = pollInterval
        self.processInspector = processInspector
    }

    func waitForHealthy(
        paths: InstallPaths,
        expectedVersion: String,
        notBefore: Date,
        timeout: Duration
    ) async throws -> Bool {
        struct RuntimeStatus: Decodable {
            let agentStatus: String
            let bridgeStatus: String
            let localHealthy: Bool
            let heartbeatAt: String
            let processID: Int32
            let appVersion: String
            let schemaVersion: Int

            enum CodingKeys: String, CodingKey {
                case agentStatus = "agent_status"
                case bridgeStatus = "bridge_status"
                case localHealthy = "local_healthy"
                case heartbeatAt = "heartbeat_at"
                case processID = "process_id"
                case appVersion = "app_version"
                case schemaVersion = "schema_version"
            }
        }

        let statusURL = paths.runURL.appendingPathComponent("runtime-status.json")
        let expectedExecutable = paths.installedAgentExecutableURL.standardizedFileURL.path
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while clock.now < deadline {
            try Task.checkCancellation()
            if let attributes = try? FileManager.default.attributesOfItem(atPath: statusURL.path),
               let modifiedAt = attributes[.modificationDate] as? Date,
               modifiedAt >= notBefore,
               let data = try? Data(contentsOf: statusURL),
               let status = try? JSONDecoder().decode(RuntimeStatus.self, from: data),
               let heartbeat = Self.parseRFC3339(status.heartbeatAt),
               heartbeat >= notBefore,
               status.schemaVersion == 1,
               status.appVersion == expectedVersion,
               status.localHealthy,
               ["unpaired", "running", "degraded"].contains(status.agentStatus),
               !status.bridgeStatus.isEmpty,
               status.processID > 0,
               let process = processInspector.inspect(processID: status.processID),
               process.userID == geteuid(),
               process.executableURL.standardizedFileURL.path == expectedExecutable,
               process.startedAt >= notBefore {
                return true
            }
            try await Task.sleep(for: pollInterval)
        }
        return false
    }

    private static func parseRFC3339(_ value: String) -> Date? {
        if let date = try? fractionalRFC3339.parse(value) { return date }
        return try? wholeSecondRFC3339.parse(value)
    }
}

@MainActor
final class ProcessRelauncher: Relaunching {
    func relaunch(executable: URL, arguments: [String]) throws {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        do { try process.run() }
        catch { throw InstallError.relaunchFailed }
    }

    func isRunning(executable: URL, startedAtOrAfter: Date) -> Bool {
        let expectedPath = executable.standardizedFileURL.path
        let currentProcessID = ProcessInfo.processInfo.processIdentifier
        return NSWorkspace.shared.runningApplications.contains { application in
            application.processIdentifier != currentProcessID
                && application.executableURL?.standardizedFileURL.path == expectedPath
                && (application.launchDate ?? .distantPast) >= startedAtOrAfter
        }
    }
}
