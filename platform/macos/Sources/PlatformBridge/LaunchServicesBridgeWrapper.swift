import AppKit
import Darwin
import Foundation

enum LaunchServicesBridgeWrapper {
    private static let launchedKey = "PCA_BRIDGE_LAUNCHED_BY_LAUNCH_SERVICES"
    private static let resultPathKey = "PCA_BRIDGE_RESULT_PATH"
    private static let wrapperPIDKey = "PCA_BRIDGE_WRAPPER_PID"

    static var shouldRelaunch: Bool {
        Bundle.main.bundleURL.pathExtension == "app"
            && ProcessInfo.processInfo.environment[launchedKey] != "1"
    }

    @MainActor
    static func run(arguments: [String]) async -> Int32 {
        let resultURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("pca-bridge-result-\(UUID().uuidString)")
        let descriptor = Darwin.open(
            resultURL.path,
            O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
            S_IRUSR | S_IWUSR
        )
        guard descriptor >= 0 else { return 1 }
        Darwin.close(descriptor)
        defer { try? FileManager.default.removeItem(at: resultURL) }

        let relay: TerminationSignalRelay
        do { relay = try TerminationSignalRelay.install() }
        catch { return 1 }

        let configuration = NSWorkspace.OpenConfiguration()
        configuration.arguments = Array(arguments.dropFirst())
        configuration.environment = [
            launchedKey: "1",
            resultPathKey: resultURL.path,
            wrapperPIDKey: String(getpid()),
        ]
        configuration.activates = false
        configuration.addsToRecentItems = false
        configuration.createsNewApplicationInstance = true

        let application: NSRunningApplication
        do {
            application = try await withCheckedThrowingContinuation { continuation in
                NSWorkspace.shared.openApplication(
                    at: Bundle.main.bundleURL,
                    configuration: configuration
                ) { application, error in
                    if let application {
                        continuation.resume(returning: application)
                    } else {
                        continuation.resume(throwing: error ?? CocoaError(.fileNoSuchFile))
                    }
                }
            }
        } catch {
            return 1
        }

        var forwardedTermination = false
        while !application.isTerminated {
            if !forwardedTermination, relay.isSignaled() {
                _ = Darwin.kill(application.processIdentifier, SIGTERM)
                forwardedTermination = true
            }
            try? await Task.sleep(for: .milliseconds(100))
        }
        guard let data = try? Data(contentsOf: resultURL),
              let value = String(data: data, encoding: .utf8),
              let status = Int32(value.trimmingCharacters(in: .whitespacesAndNewlines))
        else { return 1 }
        return status
    }

    static func startWrapperMonitorIfNeeded() {
        guard ProcessInfo.processInfo.environment[launchedKey] == "1",
              let value = ProcessInfo.processInfo.environment[wrapperPIDKey],
              let wrapperPID = Int32(value),
              wrapperPID > 1 else { return }
        Thread.detachNewThread {
            while Darwin.kill(wrapperPID, 0) == 0 || errno == EPERM {
                Thread.sleep(forTimeInterval: 0.2)
            }
            Darwin._exit(1)
        }
    }

    static func terminate(with status: Int32) -> Never {
        if let resultPath = ProcessInfo.processInfo.environment[resultPathKey] {
            let descriptor = Darwin.open(resultPath, O_WRONLY | O_TRUNC | O_CLOEXEC | O_NOFOLLOW)
            if descriptor >= 0 {
                var bytes = Array("\(status)\n".utf8)
                _ = bytes.withUnsafeMutableBytes { buffer in
                    Darwin.write(descriptor, buffer.baseAddress, buffer.count)
                }
                Darwin.close(descriptor)
            }
        }
        Darwin.exit(status)
    }
}
