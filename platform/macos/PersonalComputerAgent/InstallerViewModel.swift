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
protocol ApplicationTerminating: AnyObject {
    func terminate()
}

@MainActor
final class NSApplicationTerminator: ApplicationTerminating {
    func terminate() { NSApplication.shared.terminate(nil) }
}

@MainActor
final class InstallerViewModel: ObservableObject {
    @Published private(set) var state: InstallerState = .ready
    private let coordinator: (any InstallCoordinating)?
    private let sourceBundle: URL
    private let terminator: any ApplicationTerminating
    private var installTask: Task<Void, Never>?

    var installationAvailable: Bool { coordinator != nil }

    init(
        coordinator: any InstallCoordinating,
        sourceBundle: URL,
        terminator: any ApplicationTerminating = NSApplicationTerminator()
    ) {
        self.coordinator = coordinator
        self.sourceBundle = sourceBundle
        self.terminator = terminator
    }

    init(failureMessage: String, recoveryAction: String) {
        coordinator = nil
        sourceBundle = Bundle.main.bundleURL
        terminator = NSApplicationTerminator()
        state = .failed(message: failureMessage, recoveryAction: recoveryAction)
    }

    func installAndStart() {
        guard installTask == nil, coordinator != nil else { return }
        installTask = Task { [weak self] in
            await self?.performInstall()
        }
    }

    func performInstall() async {
        guard let coordinator else { return }
        defer { installTask = nil }
        do {
            let result = try await coordinator.installOrFinish(from: sourceBundle) { [weak self] state in
                self?.state = state
            }
            if case .relaunchRequired = result {
                terminator.terminate()
            }
        } catch let error as InstallError {
            if error.shouldTerminateCurrentProcess {
                terminator.terminate()
                return
            }
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

    func cancel() {
        installTask?.cancel()
        installTask = nil
    }
}
