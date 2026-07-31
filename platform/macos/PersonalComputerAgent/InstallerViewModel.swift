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
    private var automaticStartPending: Bool
    private var activeInstall: (generation: UUID, task: Task<Void, Never>)?

    var installationAvailable: Bool { coordinator != nil }
    var isInstalling: Bool { activeInstall != nil }

    init(
        coordinator: any InstallCoordinating,
        sourceBundle: URL,
        automaticallyStart: Bool = false,
        terminator: any ApplicationTerminating = NSApplicationTerminator()
    ) {
        self.coordinator = coordinator
        self.sourceBundle = sourceBundle
        automaticStartPending = automaticallyStart
        self.terminator = terminator
    }

    init(failureMessage: String, recoveryAction: String) {
        coordinator = nil
        sourceBundle = Bundle.main.bundleURL
        automaticStartPending = false
        terminator = NSApplicationTerminator()
        state = .failed(message: failureMessage, recoveryAction: recoveryAction)
    }

    func startIfRequested() {
        guard automaticStartPending else { return }
        automaticStartPending = false
        installAndStart()
    }

    func installAndStart() {
        guard activeInstall == nil, coordinator != nil else { return }
        let generation = UUID()
        let task = Task<Void, Never> { [weak self] in
            guard let self else { return }
            await self.performInstall(generation: generation)
        }
        activeInstall = (generation, task)
    }

    func performInstall() async {
        await runInstall()
    }

    private func performInstall(generation: UUID) async {
        defer {
            if activeInstall?.generation == generation {
                activeInstall = nil
            }
        }
        await runInstall()
    }

    private func runInstall() async {
        guard let coordinator else { return }
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
        activeInstall?.task.cancel()
    }
}
