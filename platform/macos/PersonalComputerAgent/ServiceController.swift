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
final class RuntimeHealthChecker: HealthChecking {
    private let pollInterval: Duration
    private let clockTolerance: TimeInterval
    private let processValidator: @MainActor (Int32) -> Bool

    init(
        pollInterval: Duration = .milliseconds(250),
        clockTolerance: TimeInterval = 1,
        processValidator: @escaping @MainActor (Int32) -> Bool = RuntimeHealthChecker.isLiveOwnedProcess
    ) {
        self.pollInterval = pollInterval
        self.clockTolerance = clockTolerance
        self.processValidator = processValidator
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
        let freshnessFloor = notBefore.addingTimeInterval(-clockTolerance)
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while clock.now < deadline {
            try Task.checkCancellation()
            if let attributes = try? FileManager.default.attributesOfItem(atPath: statusURL.path),
               let modifiedAt = attributes[.modificationDate] as? Date,
               modifiedAt >= freshnessFloor,
               let data = try? Data(contentsOf: statusURL),
               let status = try? JSONDecoder().decode(RuntimeStatus.self, from: data),
               let heartbeat = ISO8601DateFormatter().date(from: status.heartbeatAt),
               heartbeat >= freshnessFloor,
               status.schemaVersion == 1,
               status.appVersion == expectedVersion,
               status.localHealthy,
               ["unpaired", "running", "degraded"].contains(status.agentStatus),
               !status.bridgeStatus.isEmpty,
               status.processID > 0,
               processValidator(status.processID) {
                return true
            }
            try await Task.sleep(for: pollInterval)
        }
        return false
    }

    private static func isLiveOwnedProcess(_ processID: Int32) -> Bool {
        guard kill(processID, 0) == 0 || errno == EPERM else { return false }
        let output = Pipe()
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/ps")
        process.arguments = ["-o", "uid=", "-p", String(processID)]
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        do { try process.run() } catch { return false }
        process.waitUntilExit()
        guard process.terminationStatus == 0 else { return false }
        let raw = String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return UInt32(raw) == geteuid()
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
}
