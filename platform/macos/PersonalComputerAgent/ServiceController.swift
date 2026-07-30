import Foundation
import ServiceManagement

@MainActor
final class ServiceController: ServiceControlling {
    private let service: SMAppService
    private let approvalTimeout: Duration
    private let pollInterval: Duration

    init(
        plistName: String = BundleValidator.launchAgentName,
        approvalTimeout: Duration = .seconds(120),
        pollInterval: Duration = .milliseconds(500)
    ) {
        service = SMAppService.agent(plistName: plistName)
        self.approvalTimeout = approvalTimeout
        self.pollInterval = pollInterval
    }

    func stopAndUnregister() async throws {
        switch service.status {
        case .notRegistered, .notFound:
            return
        case .enabled, .requiresApproval:
            do {
                try await service.unregister()
            } catch {
                throw InstallError.serviceRegistrationFailed
            }
        @unknown default:
            throw InstallError.serviceRegistrationFailed
        }
    }

    func registerAndWaitForApproval(
        onWaitingForApproval: @escaping @MainActor () -> Void
    ) async throws {
        if service.status == .enabled { return }
        if service.status == .notRegistered || service.status == .notFound {
            do {
                try service.register()
            } catch {
                if service.status != .requiresApproval {
                    throw InstallError.serviceRegistrationFailed
                }
            }
        }
        guard service.status == .requiresApproval else {
            guard service.status == .enabled else { throw InstallError.serviceRegistrationFailed }
            return
        }

        onWaitingForApproval()
        SMAppService.openSystemSettingsLoginItems()
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: approvalTimeout)
        while clock.now < deadline {
            try Task.checkCancellation()
            if service.status == .enabled { return }
            try await Task.sleep(for: pollInterval)
        }
        throw InstallError.approvalTimedOut
    }
}

@MainActor
final class RuntimeHealthChecker: HealthChecking {
    func waitForHealthy(paths: InstallPaths, timeout: Duration) async throws -> Bool {
        struct RuntimeStatus: Decodable {
            let localHealthy: Bool
            let agentStatus: String

            enum CodingKeys: String, CodingKey {
                case localHealthy = "local_healthy"
                case agentStatus = "agent_status"
            }
        }

        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        let statusURL = paths.runURL.appendingPathComponent("runtime-status.json")
        while clock.now < deadline {
            try Task.checkCancellation()
            if let data = try? Data(contentsOf: statusURL),
               let status = try? JSONDecoder().decode(RuntimeStatus.self, from: data),
               status.localHealthy,
               ["unpaired", "running", "degraded"].contains(status.agentStatus) {
                return true
            }
            try await Task.sleep(for: .milliseconds(250))
        }
        return false
    }
}

@MainActor
final class ProcessRelauncher: Relaunching {
    func relaunch(executable: URL, arguments: [String]) throws {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        do {
            try process.run()
        } catch {
            throw InstallError.relaunchFailed
        }
    }
}
