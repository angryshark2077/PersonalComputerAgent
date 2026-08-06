import BridgeProtocol
import AppKit
import Darwin
import Foundation
import Security

enum InstallFailure: String, Equatable, Sendable {
    case activation
    case relaunch
    case registration
    case health
    case recovery
}

enum RollbackRecovery: String, Equatable, Sendable {
    case restoredAndRelaunched
    case restoredAndRelaunchedCleanupPending
    case restoredInactive
    case restoredInactiveCleanupPending
    case firstInstallRemoved
    case firstInstallRemovedCleanupPending
    case failed
}

enum InstallError: Error, Equatable {
    case unsafePath
    case invalidBundle
    case downgradeRejected(installed: String, candidate: String)
    case copyFailed
    case activationFailed
    case relaunchFailed
    case serviceRegistrationFailed
    case credentialProvisioningFailed
    case wechatAppDataAccessRequired
    case wechatAppDataUnavailable
    case wechatAppDataProbeFailed
    case locationAccessRequired
    case screenCaptureAccessRequired
    case photosAccessRequired
    case approvalTimedOut
    case healthCheckFailed
    case uninstallConfirmationRequired
    case keychainDeletionFailed
    case committedCleanupFailed
    case rollbackCleanupFailed
    case preparedCleanupFailed
    case transactionFailed(primary: InstallFailure, recovery: RollbackRecovery)
}

extension InstallError: LocalizedError {
    var errorDescription: String? {
        switch self {
        case .unsafePath: "The install path changed or is unsafe. No unrelated files were touched."
        case .invalidBundle: "The app bundle failed validation. Download or build a fresh signed copy."
        case let .downgradeRejected(installed, candidate):
            "Downgrade blocked: installed \(installed), candidate \(candidate). Use a newer build."
        case .copyFailed: "The app could not be staged. Check available disk space and try again."
        case .activationFailed: "The staged app could not be activated."
        case .relaunchFailed: "The installed app could not be opened."
        case .serviceRegistrationFailed: "The background service could not be registered."
        case .credentialProvisioningFailed: "The Bridge credential could not be created in Keychain."
        case .wechatAppDataAccessRequired: "Access to WeChat app data was not granted."
        case .wechatAppDataUnavailable: "The existing WeChat data directory could not be found."
        case .wechatAppDataProbeFailed: "WeChat app data access could not be verified safely."
        case .locationAccessRequired: "Location access was not granted, so Wi-Fi SSID and BSSID cannot be collected."
        case .screenCaptureAccessRequired: "Screen Recording access was not granted, so screenshots cannot be collected."
        case .photosAccessRequired: "Photos access was not granted, so photo-library originals cannot be collected."
        case .approvalTimedOut: "Background-item approval was not completed in time."
        case .healthCheckFailed: "The local runtime did not become healthy."
        case .uninstallConfirmationRequired: "Complete uninstall cancelled because the confirmation token did not match."
        case .keychainDeletionFailed: "The app files were removed, but a Keychain credential could not be deleted."
        case .committedCleanupFailed: "The update is committed, but old update artifacts could not be removed."
        case .rollbackCleanupFailed: "The previous version was restored, but recovery artifacts could not be removed."
        case .preparedCleanupFailed: "The old version was not replaced, but prepared update artifacts could not be removed."
        case let .transactionFailed(primary, recovery):
            "Installation failed during \(primary.rawValue); recovery result: \(recovery.rawValue)."
        }
    }

    var recoveryAction: String {
        switch self {
        case .wechatAppDataAccessRequired:
            "Allow PersonalComputerAgent to access data from other apps, then retry installation."
        case .wechatAppDataUnavailable:
            "Open and log in to the official WeChat once to create its data directory, quit WeChat, then retry installation."
        case .wechatAppDataProbeFailed:
            "Keep WeChat closed and retry with a fresh signed installer."
        case .locationAccessRequired:
            "Open System Settings > Privacy & Security > Location Services, enable PersonalComputerAgent, then retry."
        case .screenCaptureAccessRequired:
            "Open System Settings > Privacy & Security > Screen & System Audio Recording, enable PersonalComputerAgent, then retry."
        case .photosAccessRequired:
            "Open System Settings > Privacy & Security > Photos, allow PersonalComputerAgent full access, then retry."
        case .approvalTimedOut, .serviceRegistrationFailed:
            "Open System Settings > General > Login Items and allow Personal Computer Agent, then retry."
        case .keychainDeletionFailed:
            "Open Keychain Access and remove the Personal Computer Agent credential, then retry complete uninstall."
        case .committedCleanupFailed:
            "The new version remains active. Reopen it to retry safe cleanup; do not manually delete the install directory."
        case .rollbackCleanupFailed, .preparedCleanupFailed:
            "Reopen the signed installer to retry safe recovery cleanup. Persistent data was preserved."
        case let .transactionFailed(_, recovery):
            switch recovery {
            case .restoredAndRelaunched: "The previous version was restored and restarted; this installer will close."
            case .restoredAndRelaunchedCleanupPending:
                "The previous version was restored and restarted; cleanup will retry on its next launch."
            case .restoredInactive: "The previous version was restored without changing its prior inactive service state."
            case .restoredInactiveCleanupPending:
                "The inactive previous version was restored; cleanup will retry on its next launch."
            case .firstInstallRemoved: "The incomplete first install was removed. Persistent data was preserved."
            case .firstInstallRemovedCleanupPending:
                "The incomplete first install was removed; recovery cleanup will retry safely."
            case .failed: "Recovery did not finish. Reopen a fresh signed installer; persistent data was preserved."
            }
        case .uninstallConfirmationRequired:
            "Run the command again and enter the exact confirmation token if data deletion is intended."
        default:
            "Retry with a fresh signed installer. Persistent data has not been deleted."
        }
    }

    var shouldTerminateCurrentProcess: Bool {
        if case .transactionFailed(_, let recovery) = self {
            return recovery == .restoredAndRelaunched
                || recovery == .restoredAndRelaunchedCleanupPending
        }
        return false
    }
}

struct FileIdentity: Codable, Equatable, Sendable {
    let device: UInt64
    let inode: UInt64
    let owner: UInt32
}

struct InstallLayoutIdentity: Codable, Equatable, Sendable {
    let root: FileIdentity
    let app: FileIdentity
}

struct InstallPaths: Equatable, Sendable {
    let rootURL: URL
    let appDirectoryURL: URL
    let dataURL: URL
    let runURL: URL
    let installedBundleURL: URL
    let rollbackBundleURL: URL
    let transactionURL: URL

    var installedExecutableURL: URL {
        installedBundleURL.appendingPathComponent("Contents/MacOS/PersonalComputerAgent")
    }

    var installedAgentExecutableURL: URL {
        installedBundleURL.appendingPathComponent("Contents/Resources/bin/pca-agentd")
    }

    var installedBridgeExecutableURL: URL {
        installedBundleURL.appendingPathComponent(
            "Contents/Helpers/PCAPlatformBridge.app/Contents/MacOS/PCAPlatformBridge"
        )
    }

    var installedWechatRepairExecutableURL: URL {
        installedBundleURL.appendingPathComponent("Contents/Resources/bin/pca-wechat-repair")
    }

    static func production(fileManager: FileManager = .default) throws -> InstallPaths {
        try InstallPaths(
            rootURL: fileManager.homeDirectoryForCurrentUser
                .appendingPathComponent("Library/Application Support/PersonalComputerAgent", isDirectory: true)
        )
    }

    init(rootURL: URL) throws {
        let rawPath = rootURL.path
        guard rootURL.isFileURL,
              rawPath.hasPrefix("/"),
              rawPath != "/",
              !rawPath.split(separator: "/", omittingEmptySubsequences: false).contains("..")
        else { throw InstallError.unsafePath }

        let root = rootURL.standardizedFileURL
        guard rawPath == root.path, root.path != "/" else { throw InstallError.unsafePath }
        self.rootURL = root
        appDirectoryURL = root.appendingPathComponent("App", isDirectory: true)
        dataURL = root.appendingPathComponent("Data", isDirectory: true)
        runURL = root.appendingPathComponent("Run", isDirectory: true)
        installedBundleURL = appDirectoryURL.appendingPathComponent("PersonalComputerAgent.app", isDirectory: true)
        rollbackBundleURL = appDirectoryURL.appendingPathComponent(".rollback", isDirectory: true)
        transactionURL = appDirectoryURL.appendingPathComponent(".install-transaction.json")
        try Self.requireDirectChild(appDirectoryURL, of: root)
        try Self.requireDirectChild(dataURL, of: root)
        try Self.requireDirectChild(runURL, of: root)
        for child in [installedBundleURL, rollbackBundleURL, transactionURL] {
            try Self.requireDirectChild(child, of: appDirectoryURL)
        }
    }

    /// The parent of `rootURL` is a trusted, same-UID Application Support boundary.
    /// Every managed mutation uses lexical direct children and revalidates the exact
    /// root and App directory dev/inode/owner identities immediately beforehand.
    func prepareInstallLayout(fileManager: FileManager = .default) throws -> InstallLayoutIdentity {
        try createOrValidateDirectory(rootURL, fileManager: fileManager)
        let rootIdentity = try Self.identity(of: rootURL)
        guard rootIdentity.owner == geteuid() else { throw InstallError.unsafePath }
        try createOrValidateDirectory(appDirectoryURL, fileManager: fileManager)
        let appIdentity = try Self.identity(of: appDirectoryURL)
        guard appIdentity.owner == geteuid() else { throw InstallError.unsafePath }
        try revalidateLayout(InstallLayoutIdentity(root: rootIdentity, app: appIdentity))
        try createOrValidateDirectory(runURL, fileManager: fileManager)
        return InstallLayoutIdentity(root: rootIdentity, app: appIdentity)
    }

    func stagingBundleURL(identifier: UUID = UUID()) throws -> URL {
        let url = appDirectoryURL.appendingPathComponent(".staging-\(identifier.uuidString)", isDirectory: true)
        try Self.requireDirectChild(url, of: appDirectoryURL)
        return url
    }

    func stagingBundleURL(name: String) throws -> URL {
        guard name.hasPrefix(".staging-"), !name.contains("/") else { throw InstallError.unsafePath }
        let url = appDirectoryURL.appendingPathComponent(name, isDirectory: true)
        try Self.requireDirectChild(url, of: appDirectoryURL)
        return url
    }

    func revalidateRoot(_ expected: FileIdentity) throws {
        guard try Self.identity(of: rootURL) == expected else { throw InstallError.unsafePath }
    }

    func revalidateLayout(_ expected: InstallLayoutIdentity) throws {
        try revalidateRoot(expected.root)
        guard try Self.identity(of: appDirectoryURL) == expected.app else { throw InstallError.unsafePath }
    }

    func verifyDirectTarget(_ target: URL, parent: URL) throws {
        try Self.requireDirectChild(target, of: parent)
        if Self.entryExists(target) {
            var info = stat()
            guard lstat(target.path, &info) == 0, (info.st_mode & S_IFMT) != S_IFLNK else {
                throw InstallError.unsafePath
            }
        }
    }

    static func identity(of url: URL) throws -> FileIdentity {
        var info = stat()
        guard lstat(url.path, &info) == 0, (info.st_mode & S_IFMT) != S_IFLNK else {
            throw InstallError.unsafePath
        }
        return FileIdentity(device: UInt64(info.st_dev), inode: UInt64(info.st_ino), owner: info.st_uid)
    }

    static func entryExists(_ url: URL) -> Bool {
        var info = stat()
        return lstat(url.path, &info) == 0
    }

    private func createOrValidateDirectory(_ url: URL, fileManager: FileManager) throws {
        if Self.entryExists(url) {
            var info = stat()
            guard lstat(url.path, &info) == 0,
                  (info.st_mode & S_IFMT) == S_IFDIR,
                  info.st_uid == geteuid()
            else { throw InstallError.unsafePath }
        } else {
            try fileManager.createDirectory(at: url, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        }
        try fileManager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
    }

    private static func requireDirectChild(_ child: URL, of parent: URL) throws {
        let normalizedChild = child.standardizedFileURL
        let normalizedParent = parent.standardizedFileURL
        guard normalizedChild.deletingLastPathComponent().path == normalizedParent.path,
              normalizedChild.path != normalizedParent.path
        else { throw InstallError.unsafePath }
    }
}

struct ValidatedBundle: Equatable, Sendable {
    let version: String
    let previousVersion: String?
}

protocol BundleValidating {
    func validate(candidate: URL, replacing installed: URL?) throws -> ValidatedBundle
    func version(at bundle: URL) throws -> String
}

enum ServiceState: String, Codable, Equatable, Sendable {
    case notRegistered
    case enabled
    case requiresApproval
    case notFound
}

@MainActor
protocol ServiceControlling: AnyObject {
    func status() -> ServiceState
    func stopAndUnregister() async throws
    func registerAndWaitForApproval(onWaitingForApproval: @escaping @MainActor () -> Void) async throws
}

@MainActor
protocol HealthChecking: AnyObject {
    func waitForHealthy(
        paths: InstallPaths,
        expectedVersion: String,
        notBefore: Date,
        requiringFreshProcess: Bool,
        timeout: Duration
    ) async throws -> Bool
}

@MainActor
protocol Relaunching: AnyObject {
    func relaunch(executable: URL, arguments: [String]) throws
    func isRunning(executable: URL, startedAtOrAfter: Date) -> Bool
}

enum InstallPhase: String, Codable, Equatable, Sendable {
    case prepared
    case oldMoved
    case newActivated
    case committed
    case rolledBack
}

enum RollbackPhase: String, Codable, Equatable, Sendable {
    case oldBundleRestored
    case oldServiceRegistered
    case oldAppRelaunched
}

struct InstallTransaction: Codable, Equatable, Sendable {
    var phase: InstallPhase
    var rollbackPhase: RollbackPhase?
    var rollbackAttemptStartedAt: Date?
    let previousVersion: String?
    let candidateVersion: String
    let priorServiceState: ServiceState
    let stagingName: String
    let expectedLayoutIdentity: InstallLayoutIdentity

    init(
        phase: InstallPhase,
        previousVersion: String?,
        candidateVersion: String,
        priorServiceState: ServiceState,
        stagingName: String,
        expectedLayoutIdentity: InstallLayoutIdentity,
        rollbackPhase: RollbackPhase? = nil,
        rollbackAttemptStartedAt: Date? = nil
    ) {
        self.phase = phase
        self.rollbackPhase = rollbackPhase
        self.rollbackAttemptStartedAt = rollbackAttemptStartedAt
        self.previousVersion = previousVersion
        self.candidateVersion = candidateVersion
        self.priorServiceState = priorServiceState
        self.stagingName = stagingName
        self.expectedLayoutIdentity = expectedLayoutIdentity
    }

    func advancing(to phase: InstallPhase) -> InstallTransaction {
        var copy = self
        copy.phase = phase
        return copy
    }

    func advancingRollback(to rollbackPhase: RollbackPhase) -> InstallTransaction {
        var copy = self
        copy.rollbackPhase = rollbackPhase
        return copy
    }

    func startingRollbackAttempt(at date: Date) -> InstallTransaction {
        var copy = self
        copy.rollbackAttemptStartedAt = date
        return copy
    }
}

protocol InstallFileOperating: AnyObject {
    func exists(_ url: URL) -> Bool
    func identity(of url: URL) throws -> FileIdentity
    func copyItem(at source: URL, to destination: URL, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws
    func moveItem(at source: URL, to destination: URL, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws
    func quarantineAndDelete(_ target: URL, parent: URL, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws
    func quarantineRootChild(
        _ target: URL,
        parent: URL,
        paths: InstallPaths,
        rootIdentity: FileIdentity,
        expectedIdentity: FileIdentity
    ) throws
    func writeTransaction(_ transaction: InstallTransaction, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws
    func readTransaction(paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws -> InstallTransaction?
}

final class LocalInstallFileSystem: InstallFileOperating {
    private let fileManager: FileManager

    init(fileManager: FileManager = .default) { self.fileManager = fileManager }

    func exists(_ url: URL) -> Bool { InstallPaths.entryExists(url) }
    func identity(of url: URL) throws -> FileIdentity { try InstallPaths.identity(of: url) }

    func copyItem(
        at source: URL,
        to destination: URL,
        paths: InstallPaths,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        try paths.revalidateLayout(layoutIdentity)
        try paths.verifyDirectTarget(destination, parent: paths.appDirectoryURL)
        guard !exists(destination) else { throw InstallError.unsafePath }
        try fileManager.copyItem(at: source, to: destination)
    }

    func moveItem(
        at source: URL,
        to destination: URL,
        paths: InstallPaths,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        try paths.revalidateLayout(layoutIdentity)
        try paths.verifyDirectTarget(source, parent: paths.appDirectoryURL)
        try paths.verifyDirectTarget(destination, parent: paths.appDirectoryURL)
        guard exists(source), !exists(destination) else { throw InstallError.unsafePath }
        try fileManager.moveItem(at: source, to: destination)
    }

    func quarantineAndDelete(
        _ target: URL,
        parent: URL,
        paths: InstallPaths,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        guard exists(target) else { return }
        try paths.revalidateLayout(layoutIdentity)
        try quarantineAndDelete(
            target,
            parent: parent,
            expectedIdentity: try identity(of: target),
            revalidate: { try paths.revalidateLayout(layoutIdentity) }
        )
    }

    func quarantineRootChild(
        _ target: URL,
        parent: URL,
        paths: InstallPaths,
        rootIdentity: FileIdentity,
        expectedIdentity: FileIdentity
    ) throws {
        guard exists(target) else { return }
        try paths.revalidateRoot(rootIdentity)
        try quarantineAndDelete(
            target,
            parent: parent,
            expectedIdentity: expectedIdentity,
            revalidate: { try paths.revalidateRoot(rootIdentity) }
        )
    }

    func writeTransaction(
        _ transaction: InstallTransaction,
        paths: InstallPaths,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        try paths.revalidateLayout(layoutIdentity)
        try paths.verifyDirectTarget(paths.transactionURL, parent: paths.appDirectoryURL)
        let temporary = paths.appDirectoryURL.appendingPathComponent(".transaction-\(UUID().uuidString).tmp")
        try paths.verifyDirectTarget(temporary, parent: paths.appDirectoryURL)
        let data = try JSONEncoder().encode(transaction)
        try data.write(to: temporary, options: .withoutOverwriting)
        let handle = try FileHandle(forWritingTo: temporary)
        try handle.synchronize()
        try handle.close()
        try paths.revalidateLayout(layoutIdentity)
        guard rename(temporary.path, paths.transactionURL.path) == 0 else {
            throw InstallError.unsafePath
        }
        try paths.revalidateLayout(layoutIdentity)
        let directoryDescriptor = open(paths.appDirectoryURL.path, O_RDONLY)
        guard directoryDescriptor >= 0 else { throw InstallError.unsafePath }
        defer { close(directoryDescriptor) }
        guard fsync(directoryDescriptor) == 0 else { throw InstallError.unsafePath }
    }

    func readTransaction(
        paths: InstallPaths,
        layoutIdentity: InstallLayoutIdentity
    ) throws -> InstallTransaction? {
        try paths.revalidateLayout(layoutIdentity)
        guard exists(paths.transactionURL) else { return nil }
        try paths.verifyDirectTarget(paths.transactionURL, parent: paths.appDirectoryURL)
        return try JSONDecoder().decode(InstallTransaction.self, from: Data(contentsOf: paths.transactionURL))
    }

    private func quarantineAndDelete(
        _ target: URL,
        parent: URL,
        expectedIdentity: FileIdentity,
        revalidate: () throws -> Void
    ) throws {
        try revalidate()
        try requireDirectChild(target, parent: parent)
        guard try identity(of: target) == expectedIdentity else { throw InstallError.unsafePath }
        let quarantine = parent.appendingPathComponent(".delete-\(UUID().uuidString)")
        try requireDirectChild(quarantine, parent: parent)
        try fileManager.moveItem(at: target, to: quarantine)
        guard try identity(of: quarantine) == expectedIdentity else { throw InstallError.unsafePath }
        try revalidate()
        guard try identity(of: quarantine) == expectedIdentity else { throw InstallError.unsafePath }
        try fileManager.removeItem(at: quarantine)
    }

    private func requireDirectChild(_ child: URL, parent: URL) throws {
        guard child.standardizedFileURL.deletingLastPathComponent().path == parent.standardizedFileURL.path,
              child.standardizedFileURL.path != parent.standardizedFileURL.path
        else { throw InstallError.unsafePath }
        if exists(child), try child.resourceValues(forKeys: [.isSymbolicLinkKey]).isSymbolicLink == true {
            throw InstallError.unsafePath
        }
    }
}

enum InstallResult: Equatable, Sendable {
    case relaunchRequired(previousVersion: String?, installedVersion: String)
    case success(version: String)
}

@MainActor
protocol InstallCoordinating: AnyObject {
    func installOrFinish(from sourceBundle: URL, onState: @escaping @MainActor (InstallerState) -> Void) async throws -> InstallResult
}

@MainActor
protocol WechatAppDataAccessControlling: AnyObject {
    func waitForAuthorization(
        agentExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws
}

@MainActor
final class WechatAppDataAccessController: WechatAppDataAccessControlling {
    func waitForAuthorization(
        agentExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws {
        guard FileManager.default.isExecutableFile(atPath: agentExecutableURL.path) else {
            throw InstallError.invalidBundle
        }
        onWaitingForAuthorization()
        try Task.checkCancellation()
        let output = Pipe()
        let process = Process()
        process.executableURL = agentExecutableURL
        process.arguments = ["probe-wechat-app-data"]
        process.standardOutput = output
        process.standardError = output
        let status = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                process.terminationHandler = { process in
                    continuation.resume(returning: process.terminationStatus)
                }
                do { try process.run() } catch { continuation.resume(throwing: error) }
            }
        } onCancel: {
            if process.isRunning { process.terminate() }
        }
        try Task.checkCancellation()
        let response = String(decoding: try output.fileHandleForReading.readToEnd() ?? Data(), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        try validateProbeResult(status: status, response: response)
    }

    func validateProbeResult(status: Int32, response: String) throws {
        if status == 0, response == "authorized" || response == "not_initialized" { return }
        if status == 77, response == "permission_required" {
            throw InstallError.wechatAppDataAccessRequired
        }
        throw InstallError.wechatAppDataProbeFailed
    }
}

@MainActor
protocol LocationAccessControlling: AnyObject {
    func waitForAuthorization(
        helperExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws
}

@MainActor
protocol ScreenCaptureAccessControlling: AnyObject {
    func waitForAuthorization(
        helperExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws
}

@MainActor
protocol PhotosAccessControlling: AnyObject {
    func waitForAuthorization(
        helperExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws
}

@MainActor
final class PhotosAccessController: PhotosAccessControlling {
    func waitForAuthorization(
        helperExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws {
        guard FileManager.default.isExecutableFile(atPath: helperExecutableURL.path) else {
            throw InstallError.invalidBundle
        }
        onWaitingForAuthorization()
        let process = Process()
        process.executableURL = helperExecutableURL
        process.arguments = ["--authorize-photos"]
        let status = try await withCheckedThrowingContinuation { continuation in
            process.terminationHandler = { process in continuation.resume(returning: process.terminationStatus) }
            do { try process.run() } catch { continuation.resume(throwing: error) }
        }
        guard status == 0 else {
            if let settings = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Photos") {
                NSWorkspace.shared.open(settings)
            }
            throw InstallError.photosAccessRequired
        }
    }
}

@MainActor
final class ScreenCaptureAccessController: ScreenCaptureAccessControlling {
    func waitForAuthorization(
        helperExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws {
        guard FileManager.default.isExecutableFile(atPath: helperExecutableURL.path) else {
            throw InstallError.invalidBundle
        }
        onWaitingForAuthorization()
        try Task.checkCancellation()
        let process = Process()
        process.executableURL = helperExecutableURL
        process.arguments = ["--authorize-screen-capture"]
        let status = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                process.terminationHandler = { process in
                    continuation.resume(returning: process.terminationStatus)
                }
                do { try process.run() } catch { continuation.resume(throwing: error) }
            }
        } onCancel: {
            if process.isRunning { process.terminate() }
        }
        try Task.checkCancellation()
        guard status == 0 else {
            if let settings = URL(
                string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            ) { NSWorkspace.shared.open(settings) }
            throw InstallError.screenCaptureAccessRequired
        }
    }
}

@MainActor
final class LocationAccessController: LocationAccessControlling {
    func waitForAuthorization(
        helperExecutableURL: URL,
        onWaitingForAuthorization: @escaping @MainActor () -> Void
    ) async throws {
        guard FileManager.default.isExecutableFile(atPath: helperExecutableURL.path) else {
            throw InstallError.invalidBundle
        }
        onWaitingForAuthorization()
        try Task.checkCancellation()
        let process = Process()
        process.executableURL = helperExecutableURL
        process.arguments = ["--authorize-location"]
        let status = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                process.terminationHandler = { process in
                    continuation.resume(returning: process.terminationStatus)
                }
                do { try process.run() } catch { continuation.resume(throwing: error) }
            }
        } onCancel: {
            if process.isRunning { process.terminate() }
        }
        try Task.checkCancellation()
        guard status == 0 else {
            if let settings = URL(
                string: "x-apple.systempreferences:com.apple.preference.security?Privacy_LocationServices"
            ) { NSWorkspace.shared.open(settings) }
            throw InstallError.locationAccessRequired
        }
    }
}

@MainActor
protocol BridgeCredentialProvisioning {
    func ensureCredential(trustedApplicationURLs: [URL]) throws
    func ensureDeviceCredentialPlaceholder(trustedApplicationURLs: [URL]) throws
    func ensureWechatCredentialPlaceholder(trustedApplicationURLs: [URL]) throws
}

@MainActor
struct KeychainBridgeCredentialProvisioner: BridgeCredentialProvisioning {
    private let store = KeychainCredentialStore()

    func ensureCredential(trustedApplicationURLs: [URL]) throws {
        do {
            let secret: Data
            if let existingSecret = try store.load() {
                secret = existingSecret
            } else {
                var generatedSecret = Data(count: KeychainCredentialStore.sharedSecretLength)
                let status = generatedSecret.withUnsafeMutableBytes { buffer -> OSStatus in
                    guard let baseAddress = buffer.baseAddress else { return errSecParam }
                    return SecRandomCopyBytes(kSecRandomDefault, buffer.count, baseAddress)
                }
                guard status == errSecSuccess else {
                    throw InstallError.credentialProvisioningFailed
                }
                secret = generatedSecret
            }
            try store.store(secret, trustedApplicationURLs: trustedApplicationURLs)
        } catch {
            throw (error as? InstallError) ?? InstallError.credentialProvisioningFailed
        }
    }

    func ensureDeviceCredentialPlaceholder(trustedApplicationURLs: [URL]) throws {
        do {
            try store.ensureDeviceCredentialPlaceholder(trustedApplicationURLs: trustedApplicationURLs)
        } catch {
            throw (error as? InstallError) ?? InstallError.credentialProvisioningFailed
        }
    }

    func ensureWechatCredentialPlaceholder(trustedApplicationURLs: [URL]) throws {
        do {
            try store.ensureWechatCredentialPlaceholder(trustedApplicationURLs: trustedApplicationURLs)
        } catch {
            throw (error as? InstallError) ?? InstallError.credentialProvisioningFailed
        }
    }
}

@MainActor
final class InstallCoordinator: InstallCoordinating {
    private let paths: InstallPaths
    private let validator: any BundleValidating
    private let service: any ServiceControlling
    private let health: any HealthChecking
    private let relauncher: any Relaunching
    private let wechatAppDataAccess: any WechatAppDataAccessControlling
    private let locationAccess: any LocationAccessControlling
    private let screenCaptureAccess: any ScreenCaptureAccessControlling
    private let photosAccess: any PhotosAccessControlling
    private let credentialProvisioner: any BridgeCredentialProvisioning
    private let fileSystem: any InstallFileOperating

    init(
        paths: InstallPaths,
        validator: any BundleValidating,
        service: any ServiceControlling,
        health: any HealthChecking,
        relauncher: any Relaunching,
        wechatAppDataAccess: any WechatAppDataAccessControlling = WechatAppDataAccessController(),
        locationAccess: any LocationAccessControlling = LocationAccessController(),
        screenCaptureAccess: any ScreenCaptureAccessControlling = ScreenCaptureAccessController(),
        photosAccess: any PhotosAccessControlling = PhotosAccessController(),
        credentialProvisioner: any BridgeCredentialProvisioning = KeychainBridgeCredentialProvisioner(),
        fileSystem: any InstallFileOperating = LocalInstallFileSystem()
    ) {
        self.paths = paths
        self.validator = validator
        self.service = service
        self.health = health
        self.relauncher = relauncher
        self.wechatAppDataAccess = wechatAppDataAccess
        self.locationAccess = locationAccess
        self.screenCaptureAccess = screenCaptureAccess
        self.photosAccess = photosAccess
        self.credentialProvisioner = credentialProvisioner
        self.fileSystem = fileSystem
    }

    func installOrFinish(
        from sourceBundle: URL,
        onState: @escaping @MainActor (InstallerState) -> Void = { _ in }
    ) async throws -> InstallResult {
        if sourceBundle.standardizedFileURL.path == paths.installedBundleURL.standardizedFileURL.path {
            return try await finishInstalledSetup(onState: onState)
        }
        return try await prepareInstallation(from: sourceBundle, onState: onState)
    }

    func prepareInstallation(
        from sourceBundle: URL,
        onState: @escaping @MainActor (InstallerState) -> Void = { _ in }
    ) async throws -> InstallResult {
        let layoutIdentity = try paths.prepareInstallLayout()
        var inheritedPriorServiceState: ServiceState?
        if let pending = try checkedTransaction(layoutIdentity: layoutIdentity) {
            if pending.phase == .prepared,
               try preparedOldBundleIsUntouched(pending) {
                do { try cleanupPrepared(pending, layoutIdentity: layoutIdentity) }
                catch { throw InstallError.preparedCleanupFailed }
                inheritedPriorServiceState = pending.priorServiceState
            } else if let recovery = try await recoverPendingInstallation(layoutIdentity: layoutIdentity) {
                throw InstallError.transactionFailed(primary: .recovery, recovery: recovery)
            }
        }
        let staging = try paths.stagingBundleURL()
        let installedExists = fileSystem.exists(paths.installedBundleURL)

        if fileSystem.exists(paths.rollbackBundleURL) {
            try fileSystem.quarantineAndDelete(
                paths.rollbackBundleURL,
                parent: paths.appDirectoryURL,
                paths: paths,
                layoutIdentity: layoutIdentity
            )
        }

        onState(.copying)
        do {
            try fileSystem.copyItem(
                at: sourceBundle,
                to: staging,
                paths: paths,
                layoutIdentity: layoutIdentity
            )
        } catch let error as InstallError { throw error }
        catch {
            do { try cleanupUnjournaledStaging(staging, layoutIdentity: layoutIdentity) }
            catch { throw InstallError.preparedCleanupFailed }
            throw InstallError.copyFailed
        }

        onState(.validating)
        let validated: ValidatedBundle
        do {
            validated = try validator.validate(candidate: staging, replacing: installedExists ? paths.installedBundleURL : nil)
        } catch {
            do { try cleanupUnjournaledStaging(staging, layoutIdentity: layoutIdentity) }
            catch { throw InstallError.preparedCleanupFailed }
            throw error
        }

        var transaction = InstallTransaction(
            phase: .prepared,
            previousVersion: validated.previousVersion,
            candidateVersion: validated.version,
            priorServiceState: inheritedPriorServiceState ?? service.status(),
            stagingName: staging.lastPathComponent,
            expectedLayoutIdentity: layoutIdentity
        )
        do {
            try fileSystem.writeTransaction(transaction, paths: paths, layoutIdentity: layoutIdentity)
        } catch {
            do { try cleanupUnjournaledStaging(staging, layoutIdentity: layoutIdentity) }
            catch { throw InstallError.preparedCleanupFailed }
            throw error
        }

        do {
            try await service.stopAndUnregister()
        } catch {
            do { try cleanupPrepared(transaction, layoutIdentity: layoutIdentity) }
            catch { throw InstallError.preparedCleanupFailed }
            throw InstallError.serviceRegistrationFailed
        }

        do {
            if installedExists {
                try fileSystem.moveItem(
                    at: paths.installedBundleURL,
                    to: paths.rollbackBundleURL,
                    paths: paths,
                    layoutIdentity: layoutIdentity
                )
            }
            transaction = transaction.advancing(to: .oldMoved)
            try fileSystem.writeTransaction(transaction, paths: paths, layoutIdentity: layoutIdentity)
            try fileSystem.moveItem(
                at: staging,
                to: paths.installedBundleURL,
                paths: paths,
                layoutIdentity: layoutIdentity
            )
            transaction = transaction.advancing(to: .newActivated)
            try fileSystem.writeTransaction(transaction, paths: paths, layoutIdentity: layoutIdentity)
        } catch {
            let recovery = await recoverFailure(transaction, layoutIdentity: layoutIdentity)
            throw InstallError.transactionFailed(primary: .activation, recovery: recovery)
        }

        do {
            try relauncher.relaunch(executable: paths.installedExecutableURL, arguments: ["--setup-installed"])
        } catch {
            let recovery = await recoverFailure(transaction, layoutIdentity: layoutIdentity)
            throw InstallError.transactionFailed(primary: .relaunch, recovery: recovery)
        }
        return .relaunchRequired(previousVersion: validated.previousVersion, installedVersion: validated.version)
    }

    func finishInstalledSetup(
        onState: @escaping @MainActor (InstallerState) -> Void = { _ in }
    ) async throws -> InstallResult {
        let layoutIdentity = try paths.prepareInstallLayout()
        let transaction = try checkedTransaction(layoutIdentity: layoutIdentity)
        if let existing = transaction {
            switch existing.phase {
            case .committed:
                try cleanupCommitted(existing, layoutIdentity: layoutIdentity)
                return .success(version: existing.candidateVersion)
            case .rolledBack:
                try cleanupRolledBack(existing, layoutIdentity: layoutIdentity)
                return .success(version: existing.previousVersion ?? existing.candidateVersion)
            case .prepared, .oldMoved:
                let recovery = try await recoverPendingInstallation(layoutIdentity: layoutIdentity) ?? .failed
                throw InstallError.transactionFailed(primary: .recovery, recovery: recovery)
            case .newActivated:
                break
            }
        }

        let installedVersion = try validator.version(at: paths.installedBundleURL)
        try await wechatAppDataAccess.waitForAuthorization(
            agentExecutableURL: paths.installedAgentExecutableURL,
            onWaitingForAuthorization: { onState(.waitingWechatAppDataAccess) }
        )
        try await locationAccess.waitForAuthorization(helperExecutableURL: paths.installedBridgeExecutableURL) {
            onState(.waitingLocationAccess)
        }
        try await screenCaptureAccess.waitForAuthorization(helperExecutableURL: paths.installedBridgeExecutableURL) {
            onState(.waitingScreenCaptureAccess)
        }
        try await photosAccess.waitForAuthorization(helperExecutableURL: paths.installedBridgeExecutableURL) {
            onState(.waitingPhotosAccess)
        }
        do {
            let trustedApplicationURLs = [
                paths.installedBundleURL,
                paths.installedAgentExecutableURL,
                paths.installedBridgeExecutableURL,
            ]
            try credentialProvisioner.ensureCredential(trustedApplicationURLs: trustedApplicationURLs)
            try credentialProvisioner.ensureDeviceCredentialPlaceholder(trustedApplicationURLs: trustedApplicationURLs)
            try credentialProvisioner.ensureWechatCredentialPlaceholder(trustedApplicationURLs: [
                paths.installedAgentExecutableURL,
                paths.installedWechatRepairExecutableURL,
            ])
        } catch {
            if let transaction {
                let recovery = await recoverFailure(transaction, layoutIdentity: layoutIdentity)
                throw InstallError.transactionFailed(primary: .registration, recovery: recovery)
            }
            throw (error as? InstallError) ?? InstallError.credentialProvisioningFailed
        }
        let attemptStartedAt = Date()
        let existingRuntimeIsEnabled = service.status() == .enabled
        onState(.starting)
        if !existingRuntimeIsEnabled {
            do {
                try await service.registerAndWaitForApproval { onState(.waitingApproval) }
            } catch {
                if let transaction {
                    let recovery = await recoverFailure(transaction, layoutIdentity: layoutIdentity)
                    throw InstallError.transactionFailed(primary: .registration, recovery: recovery)
                }
                throw (error as? InstallError) ?? InstallError.serviceRegistrationFailed
            }
        }

        let healthy: Bool
        do {
            healthy = try await health.waitForHealthy(
                paths: paths,
                expectedVersion: installedVersion,
                notBefore: existingRuntimeIsEnabled
                    ? attemptStartedAt.addingTimeInterval(-60)
                    : attemptStartedAt,
                requiringFreshProcess: !existingRuntimeIsEnabled,
                timeout: .seconds(5)
            )
        } catch {
            if let transaction {
                let recovery = await recoverFailure(transaction, layoutIdentity: layoutIdentity)
                throw InstallError.transactionFailed(primary: .health, recovery: recovery)
            }
            throw error
        }
        guard healthy else {
            if let transaction {
                let recovery = await recoverFailure(transaction, layoutIdentity: layoutIdentity)
                throw InstallError.transactionFailed(primary: .health, recovery: recovery)
            }
            throw InstallError.healthCheckFailed
        }

        if let existing = transaction {
            let committed = existing.advancing(to: .committed)
            do { try fileSystem.writeTransaction(committed, paths: paths, layoutIdentity: layoutIdentity) }
            catch {
                let recovery = await recoverFailure(existing, layoutIdentity: layoutIdentity)
                throw InstallError.transactionFailed(primary: .health, recovery: recovery)
            }
            try cleanupCommitted(committed, layoutIdentity: layoutIdentity)
        }
        onState(.success)
        return .success(version: installedVersion)
    }

    func recoverPendingInstallation() async throws -> RollbackRecovery? {
        let layoutIdentity = try paths.prepareInstallLayout()
        return try await recoverPendingInstallation(layoutIdentity: layoutIdentity)
    }

    private func recoverPendingInstallation(
        layoutIdentity: InstallLayoutIdentity
    ) async throws -> RollbackRecovery? {
        guard let transaction = try checkedTransaction(layoutIdentity: layoutIdentity) else { return nil }
        switch transaction.phase {
        case .prepared:
            if transaction.previousVersion != nil,
               !fileSystem.exists(paths.installedBundleURL),
               fileSystem.exists(paths.rollbackBundleURL) {
                return await recoverFailure(transaction.advancing(to: .oldMoved), layoutIdentity: layoutIdentity)
            }
            do { try cleanupPrepared(transaction, layoutIdentity: layoutIdentity) }
            catch { throw InstallError.preparedCleanupFailed }
            return nil
        case .oldMoved, .newActivated:
            return await recoverFailure(transaction, layoutIdentity: layoutIdentity)
        case .committed:
            try cleanupCommitted(transaction, layoutIdentity: layoutIdentity)
            return nil
        case .rolledBack:
            try cleanupRolledBack(transaction, layoutIdentity: layoutIdentity)
            return nil
        }
    }

    private func recoverFailure(
        _ transaction: InstallTransaction,
        layoutIdentity: InstallLayoutIdentity
    ) async -> RollbackRecovery {
        do {
            var transaction = transaction
            let recovery: RollbackRecovery
            if let previousVersion = transaction.previousVersion {
                try await service.stopAndUnregister()
                try restorePreviousBundle(previousVersion, layoutIdentity: layoutIdentity)
                if transaction.rollbackPhase == nil {
                    transaction = transaction.advancingRollback(to: .oldBundleRestored)
                    try fileSystem.writeTransaction(
                        transaction,
                        paths: paths,
                        layoutIdentity: layoutIdentity
                    )
                }

                let rolledBack = transaction.advancing(to: .rolledBack)
                try fileSystem.writeTransaction(rolledBack, paths: paths, layoutIdentity: layoutIdentity)
                do { try cleanupRolledBack(rolledBack, layoutIdentity: layoutIdentity) }
                catch {
                    return .restoredInactiveCleanupPending
                }

                if transaction.priorServiceState == .enabled {
                    do {
                        try relauncher.relaunch(
                            executable: paths.installedExecutableURL,
                            arguments: ["--setup-installed"]
                        )
                        recovery = .restoredAndRelaunched
                    } catch {
                        recovery = .restoredInactive
                    }
                } else {
                    recovery = .restoredInactive
                }
                return recovery
            } else {
                try await service.stopAndUnregister()
                if fileSystem.exists(paths.installedBundleURL) {
                    try fileSystem.quarantineAndDelete(
                        paths.installedBundleURL,
                        parent: paths.appDirectoryURL,
                        paths: paths,
                        layoutIdentity: layoutIdentity
                    )
                }
                if fileSystem.exists(paths.runURL) {
                    try fileSystem.quarantineAndDelete(
                        paths.runURL,
                        parent: paths.rootURL,
                        paths: paths,
                        layoutIdentity: layoutIdentity
                    )
                }
                recovery = .firstInstallRemoved
            }

            let rolledBack = transaction.advancing(to: .rolledBack)
            try fileSystem.writeTransaction(rolledBack, paths: paths, layoutIdentity: layoutIdentity)
            do { try cleanupRolledBack(rolledBack, layoutIdentity: layoutIdentity) }
            catch {
                switch recovery {
                case .restoredAndRelaunched: return .restoredAndRelaunchedCleanupPending
                case .restoredInactive: return .restoredInactiveCleanupPending
                case .firstInstallRemoved: return .firstInstallRemovedCleanupPending
                default: return .failed
                }
            }
            return recovery
        } catch {
            return .failed
        }
    }

    private func restorePreviousBundle(
        _ previousVersion: String,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        if fileSystem.exists(paths.rollbackBundleURL) {
            if fileSystem.exists(paths.installedBundleURL) {
                try fileSystem.quarantineAndDelete(
                    paths.installedBundleURL,
                    parent: paths.appDirectoryURL,
                    paths: paths,
                    layoutIdentity: layoutIdentity
                )
            }
            try fileSystem.moveItem(
                at: paths.rollbackBundleURL,
                to: paths.installedBundleURL,
                paths: paths,
                layoutIdentity: layoutIdentity
            )
        }
        guard fileSystem.exists(paths.installedBundleURL),
              try validator.version(at: paths.installedBundleURL) == previousVersion
        else { throw InstallError.activationFailed }
    }

    private func checkedTransaction(
        layoutIdentity: InstallLayoutIdentity
    ) throws -> InstallTransaction? {
        let transaction = try fileSystem.readTransaction(paths: paths, layoutIdentity: layoutIdentity)
        if let transaction {
            guard transaction.expectedLayoutIdentity == layoutIdentity else { throw InstallError.unsafePath }
        }
        return transaction
    }

    private func preparedOldBundleIsUntouched(_ transaction: InstallTransaction) throws -> Bool {
        guard !fileSystem.exists(paths.rollbackBundleURL) else { return false }
        if let previousVersion = transaction.previousVersion {
            return try fileSystem.exists(paths.installedBundleURL)
                && (try validator.version(at: paths.installedBundleURL)) == previousVersion
        }
        return !fileSystem.exists(paths.installedBundleURL)
    }

    private func cleanupUnjournaledStaging(
        _ staging: URL,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        if fileSystem.exists(staging) {
            try fileSystem.quarantineAndDelete(
                staging,
                parent: paths.appDirectoryURL,
                paths: paths,
                layoutIdentity: layoutIdentity
            )
        }
    }

    private func cleanupPrepared(
        _ transaction: InstallTransaction,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        let staging = try paths.stagingBundleURL(name: transaction.stagingName)
        try cleanupUnjournaledStaging(staging, layoutIdentity: layoutIdentity)
        try removeTransaction(layoutIdentity: layoutIdentity)
    }

    private func cleanupCommitted(
        _ transaction: InstallTransaction,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        do {
            let staging = try paths.stagingBundleURL(name: transaction.stagingName)
            try cleanupUnjournaledStaging(staging, layoutIdentity: layoutIdentity)
            if fileSystem.exists(paths.rollbackBundleURL) {
                try fileSystem.quarantineAndDelete(
                    paths.rollbackBundleURL,
                    parent: paths.appDirectoryURL,
                    paths: paths,
                    layoutIdentity: layoutIdentity
                )
            }
            try removeTransaction(layoutIdentity: layoutIdentity)
        } catch {
            throw InstallError.committedCleanupFailed
        }
    }

    private func cleanupRolledBack(
        _ transaction: InstallTransaction,
        layoutIdentity: InstallLayoutIdentity
    ) throws {
        do {
            let staging = try paths.stagingBundleURL(name: transaction.stagingName)
            try cleanupUnjournaledStaging(staging, layoutIdentity: layoutIdentity)
            if fileSystem.exists(paths.rollbackBundleURL) {
                try fileSystem.quarantineAndDelete(
                    paths.rollbackBundleURL,
                    parent: paths.appDirectoryURL,
                    paths: paths,
                    layoutIdentity: layoutIdentity
                )
            }
            try removeTransaction(layoutIdentity: layoutIdentity)
        } catch {
            throw InstallError.rollbackCleanupFailed
        }
    }

    private func removeTransaction(layoutIdentity: InstallLayoutIdentity) throws {
        if fileSystem.exists(paths.transactionURL) {
            try fileSystem.quarantineAndDelete(
                paths.transactionURL,
                parent: paths.appDirectoryURL,
                paths: paths,
                layoutIdentity: layoutIdentity
            )
        }
    }
}

struct Version: Equatable, Sendable {
    private let components: [Int]

    var isValid: Bool { components.count == 3 && components.allSatisfy { $0 >= 0 } }

    init(_ raw: String) {
        let pieces = raw.split(separator: ".", omittingEmptySubsequences: false)
        components = pieces.map { piece in
            guard !piece.isEmpty, piece.allSatisfy(\.isNumber), let value = Int(piece) else { return -1 }
            return value
        }
    }

    func compare(to other: Version) -> ComparisonResult {
        for index in 0..<max(components.count, other.components.count) {
            let lhs = index < components.count ? components[index] : 0
            let rhs = index < other.components.count ? other.components[index] : 0
            if lhs < rhs { return .orderedAscending }
            if lhs > rhs { return .orderedDescending }
        }
        return .orderedSame
    }
}
