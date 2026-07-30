import AppKit
import Foundation

enum InstallerState: Equatable, Sendable {
    case ready
    case copying
    case validating
    case waitingApproval
    case starting
    case success
    case failed(message: String, recoveryAction: String)
}

@MainActor
final class InstallerViewModel: ObservableObject {
    @Published private(set) var state: InstallerState = .ready
    private let coordinator: InstallCoordinator?
    private let sourceBundle: URL
    private var installTask: Task<Void, Never>?

    var installationAvailable: Bool { coordinator != nil }

    init(coordinator: InstallCoordinator, sourceBundle: URL) {
        self.coordinator = coordinator
        self.sourceBundle = sourceBundle
    }

    init(failureMessage: String, recoveryAction: String) {
        coordinator = nil
        sourceBundle = Bundle.main.bundleURL
        state = .failed(message: failureMessage, recoveryAction: recoveryAction)
    }

    func installAndStart() {
        guard installTask == nil, let coordinator else { return }
        installTask = Task {
            defer { installTask = nil }
            do {
                let result = try await coordinator.installOrFinish(from: sourceBundle) { [weak self] state in
                    self?.state = state
                }
                if case .relaunchRequired = result {
                    NSApplication.shared.terminate(nil)
                }
            } catch let error as InstallError {
                state = .failed(
                    message: error.localizedDescription,
                    recoveryAction: error.recoveryAction
                )
            } catch {
                state = .failed(
                    message: "Installation failed without changing persistent data.",
                    recoveryAction: "Retry with a fresh signed installer."
                )
            }
        }
    }

    func cancel() {
        installTask?.cancel()
        installTask = nil
    }
}
