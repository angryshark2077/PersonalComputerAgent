import AppKit
import Darwin
import Foundation

enum InstallerState: Equatable, Sendable {
    case ready
    case copying
    case validating
    case waitingWechatAppDataAccess
    case waitingLocationAccess
    case waitingScreenCaptureAccess
    case waitingPhotosAccess
    case migratingKeychainAccess
    case waitingApproval
    case starting
    case pairing
    case preparingAutomaticWechatRecovery
    case automaticWechatRecoveryPrepared
    case automaticWechatRecoveryDeferred(message: String)
    case automaticWechatRecoveryFailed(message: String)
    case repair(message: String)
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
protocol WechatRepairRunning: AnyObject {
    var isAvailable: Bool { get }
    func prepareAutomaticRecovery() async throws
}

enum WechatRepairRunnerError: LocalizedError, Equatable {
    case unavailable
    case notApplicable
    case requiresUserAction(message: String)
    case failed(message: String)

    var errorDescription: String? {
        switch self {
        case .unavailable:
            "The installed WeChat recovery tool is unavailable."
        case .notApplicable:
            "Automatic WeChat recovery is not available on this Mac."
        case let .requiresUserAction(message):
            message
        case let .failed(message):
            message
        }
    }
}

private final class ProcessCancellationState: @unchecked Sendable {
    private let lock = NSLock()
    private var requested = false

    func request() {
        lock.withLock { requested = true }
    }

    var isRequested: Bool {
        lock.withLock { requested }
    }
}

private func terminateProcessGroup(_ process: Process) {
    guard process.isRunning else { return }
    let processIdentifier = process.processIdentifier
    if processIdentifier > 0 {
        _ = Darwin.kill(-processIdentifier, SIGTERM)
    }
    process.terminate()
}

@MainActor
final class ProcessWechatRepairRunner: WechatRepairRunning {
    private let executableURL: URL

    init(executableURL: URL) {
        self.executableURL = executableURL
    }

    var isAvailable: Bool {
        FileManager.default.isExecutableFile(atPath: executableURL.path)
    }

    func prepareAutomaticRecovery() async throws {
        guard isAvailable else { throw WechatRepairRunnerError.unavailable }
        let output = Pipe()
        let process = Process()
        process.executableURL = executableURL
        process.arguments = ["prepare-automatic"]
        process.environment = ProcessInfo.processInfo.environment.merging([
            "PCA_INSTALLER_PID": String(ProcessInfo.processInfo.processIdentifier),
        ]) { _, current in current }
        process.standardOutput = output
        process.standardError = output
        let cancellationState = ProcessCancellationState()
        try Task.checkCancellation()
        let status = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Int32, Error>) in
                guard !cancellationState.isRequested else {
                    continuation.resume(throwing: CancellationError())
                    return
                }
                process.terminationHandler = { process in
                    continuation.resume(returning: process.terminationStatus)
                }
                do {
                    try process.run()
                    if cancellationState.isRequested {
                        terminateProcessGroup(process)
                    }
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        } onCancel: {
            cancellationState.request()
            terminateProcessGroup(process)
        }
        try Task.checkCancellation()
        let data = try output.fileHandleForReading.readToEnd() ?? Data()
        let message = String(decoding: data, as: UTF8.self)
            .split(whereSeparator: \.isNewline)
            .last
            .map(String.init)
            ?? "WeChat key recovery failed safely."
        if [3, 4, 6].contains(status) {
            throw WechatRepairRunnerError.notApplicable
        }
        if status == 9 {
            throw WechatRepairRunnerError.requiresUserAction(message: message)
        }
        guard status == 0 else {
            throw WechatRepairRunnerError.failed(message: message)
        }
    }
}

@MainActor
final class InstallerViewModel: ObservableObject {
    @Published private(set) var state: InstallerState = .ready
    private let coordinator: (any InstallCoordinating)?
    private let sourceBundle: URL
    private let terminator: any ApplicationTerminating
    private let pairingCoordinator: PairingCoordinator?
    private let wechatRepairRunner: (any WechatRepairRunning)?
    private let terminateAfterSuccessfulSetup: Bool
    private var automaticStartPending: Bool
    private var activeInstall: (generation: UUID, task: Task<Void, Never>)?
    private var activePairing: Task<Void, Never>?
    private var activeWechatRepair: Task<Void, Never>?

    var installationAvailable: Bool { coordinator != nil }
    var isInstalling: Bool { activeInstall != nil }
    var isPairing: Bool { activePairing != nil }
    var isPreparingAutomaticWechatRecovery: Bool { activeWechatRepair != nil }
    var wechatRepairAvailable: Bool { wechatRepairRunner?.isAvailable == true }

    init(
        coordinator: any InstallCoordinating,
        sourceBundle: URL,
        automaticallyStart: Bool = false,
        pairingCoordinator: PairingCoordinator? = nil,
        wechatRepairRunner: (any WechatRepairRunning)? = nil,
        terminator: any ApplicationTerminating = NSApplicationTerminator()
    ) {
        self.coordinator = coordinator
        self.sourceBundle = sourceBundle
        automaticStartPending = automaticallyStart
        terminateAfterSuccessfulSetup = automaticallyStart
        self.pairingCoordinator = pairingCoordinator
        self.wechatRepairRunner = wechatRepairRunner
        self.terminator = terminator
    }

    init(failureMessage: String, recoveryAction: String) {
        coordinator = nil
        sourceBundle = Bundle.main.bundleURL
        automaticStartPending = false
        terminateAfterSuccessfulSetup = false
        pairingCoordinator = nil
        wechatRepairRunner = nil
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
            } else if pairingCoordinator != nil {
                startPairing(repair: false)
            } else if wechatRepairAvailable {
                startAutomaticWechatRecoveryPreparation()
            } else {
                state = .success
                if terminateAfterSuccessfulSetup { terminator.terminate() }
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

    func repairPairing() {
        startPairing(repair: true)
    }

    func cancelPairing() {
        guard activePairing != nil, let pairingCoordinator else { return }
        activePairing?.cancel()
        Task { await pairingCoordinator.cancel() }
    }

    func retryAutomaticWechatRecoveryAuthorization() {
        startAutomaticWechatRecoveryPreparation()
    }

    private func startAutomaticWechatRecoveryPreparation() {
        guard activeWechatRepair == nil, let wechatRepairRunner else { return }
        state = .preparingAutomaticWechatRecovery
        activeWechatRepair = Task { [weak self] in
            defer { self?.activeWechatRepair = nil }
            do {
                try await wechatRepairRunner.prepareAutomaticRecovery()
                self?.state = .automaticWechatRecoveryPrepared
                if self?.terminateAfterSuccessfulSetup == true {
                    self?.terminator.terminate()
                }
            } catch WechatRepairRunnerError.notApplicable {
                self?.state = .success
                if self?.terminateAfterSuccessfulSetup == true {
                    self?.terminator.terminate()
                }
            } catch let WechatRepairRunnerError.requiresUserAction(message) {
                self?.state = .automaticWechatRecoveryDeferred(message: message)
            } catch {
                self?.state = .automaticWechatRecoveryFailed(
                    message: (error as? LocalizedError)?.errorDescription
                        ?? "Automatic WeChat recovery could not be prepared safely."
                )
            }
        }
    }

    private func startPairing(repair: Bool) {
        guard activePairing == nil, let pairingCoordinator else { return }
        state = .pairing
        activePairing = Task { [weak self] in
            defer { self?.activePairing = nil }
            do {
                _ = try await (repair ? pairingCoordinator.repair() : pairingCoordinator.startIfUnpaired())
                if !repair, self?.wechatRepairAvailable == true {
                    self?.startAutomaticWechatRecoveryPreparation()
                } else {
                    self?.state = .success
                }
                if self?.terminateAfterSuccessfulSetup == true,
                   (repair || self?.wechatRepairAvailable != true) {
                    self?.terminator.terminate()
                }
            } catch let error as PairingError {
                self?.state = .repair(message: error.localizedDescription)
            } catch {
                self?.state = .repair(message: "Pairing could not be completed safely.")
            }
        }
    }
}
