import Darwin
import Foundation

enum InstallFailure: String, Equatable, Sendable {
    case activation
    case relaunch
    case registration
    case health
}

enum RollbackRecovery: String, Equatable, Sendable {
    case restoredAndRelaunched
    case restoredInactive
    case firstInstallRemoved
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
    case approvalTimedOut
    case healthCheckFailed
    case uninstallConfirmationRequired
    case keychainDeletionFailed
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
        case .approvalTimedOut: "Background-item approval was not completed in time."
        case .healthCheckFailed: "The local runtime did not become healthy."
        case .uninstallConfirmationRequired: "Complete uninstall cancelled because the confirmation token did not match."
        case .keychainDeletionFailed: "The app files were removed, but a Keychain credential could not be deleted."
        case let .transactionFailed(primary, recovery):
            "Installation failed during \(primary.rawValue); recovery result: \(recovery.rawValue)."
        }
    }

    var recoveryAction: String {
        switch self {
        case .approvalTimedOut, .serviceRegistrationFailed:
            "Open System Settings > General > Login Items and allow Personal Computer Agent, then retry."
        case .keychainDeletionFailed:
            "Open Keychain Access and remove the Personal Computer Agent credential, then retry complete uninstall."
        case let .transactionFailed(_, recovery):
            switch recovery {
            case .restoredAndRelaunched: "The previous version was restored and restarted; this installer will close."
            case .restoredInactive: "The previous version was restored without changing its prior disabled service state."
            case .firstInstallRemoved: "The incomplete first install was removed. Persistent data was preserved."
            case .failed: "Recovery did not finish. Reopen a fresh signed installer; persistent data was preserved."
            }
        case .uninstallConfirmationRequired:
            "Run the command again and enter the exact confirmation token if data deletion is intended."
        default:
            "Retry with a fresh signed installer. Persistent data has not been deleted."
        }
    }

    var shouldTerminateCurrentProcess: Bool {
        if case .transactionFailed(_, .restoredAndRelaunched) = self { return true }
        return false
    }
}

struct FileIdentity: Codable, Equatable, Sendable {
    let device: UInt64
    let inode: UInt64
    let owner: UInt32
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
        guard rawPath == root.path,
              root.path != "/"
        else { throw InstallError.unsafePath }
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

    /// The parent of `rootURL` is a trusted, user-owned Application Support boundary.
    /// Within the managed root, every mutation is lexical, direct-child-only, and
    /// revalidates the captured root identity immediately before the operation.
    func prepareInstallLayout(fileManager: FileManager = .default) throws -> FileIdentity {
        try createOrValidateDirectory(rootURL, fileManager: fileManager)
        let identity = try Self.identity(of: rootURL)
        guard identity.owner == geteuid() else { throw InstallError.unsafePath }
        for directory in [appDirectoryURL, runURL] {
            try revalidateRoot(identity)
            try createOrValidateDirectory(directory, fileManager: fileManager)
        }
        return identity
    }

    func stagingBundleURL(identifier: UUID = UUID()) throws -> URL {
        let url = appDirectoryURL.appendingPathComponent(".staging-\(identifier.uuidString)", isDirectory: true)
        try Self.requireDirectChild(url, of: appDirectoryURL)
        return url
    }

    func revalidateRoot(_ expected: FileIdentity) throws {
        guard try Self.identity(of: rootURL) == expected else { throw InstallError.unsafePath }
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
        guard lstat(url.path, &info) == 0, (info.st_mode & S_IFMT) == S_IFDIR else {
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
    func waitForHealthy(paths: InstallPaths, expectedVersion: String, notBefore: Date, timeout: Duration) async throws -> Bool
}

@MainActor
protocol Relaunching: AnyObject {
    func relaunch(executable: URL, arguments: [String]) throws
}

struct InstallTransaction: Codable, Equatable, Sendable {
    let previousVersion: String?
    let candidateVersion: String
    let priorServiceState: ServiceState
}

protocol InstallFileOperating: AnyObject {
    func exists(_ url: URL) -> Bool
    func copyItem(at source: URL, to destination: URL) throws
    func moveItem(at source: URL, to destination: URL, paths: InstallPaths, rootIdentity: FileIdentity) throws
    func quarantineAndDelete(_ target: URL, parent: URL, paths: InstallPaths, rootIdentity: FileIdentity) throws
    func writeTransaction(_ transaction: InstallTransaction, paths: InstallPaths, rootIdentity: FileIdentity) throws
    func readTransaction(paths: InstallPaths, rootIdentity: FileIdentity) throws -> InstallTransaction?
}

final class LocalInstallFileSystem: InstallFileOperating {
    private let fileManager: FileManager

    init(fileManager: FileManager = .default) { self.fileManager = fileManager }

    func exists(_ url: URL) -> Bool { InstallPaths.entryExists(url) }

    func copyItem(at source: URL, to destination: URL) throws {
        guard !exists(destination) else { throw InstallError.unsafePath }
        try fileManager.copyItem(at: source, to: destination)
    }

    func moveItem(at source: URL, to destination: URL, paths: InstallPaths, rootIdentity: FileIdentity) throws {
        try paths.revalidateRoot(rootIdentity)
        try paths.verifyDirectTarget(source, parent: paths.appDirectoryURL)
        try paths.verifyDirectTarget(destination, parent: paths.appDirectoryURL)
        guard exists(source), !exists(destination) else { throw InstallError.unsafePath }
        try fileManager.moveItem(at: source, to: destination)
    }

    func quarantineAndDelete(_ target: URL, parent: URL, paths: InstallPaths, rootIdentity: FileIdentity) throws {
        guard exists(target) else { return }
        try paths.revalidateRoot(rootIdentity)
        try paths.verifyDirectTarget(target, parent: parent)
        let original = try entryIdentity(target)
        let quarantine = parent.appendingPathComponent(".delete-\(UUID().uuidString)")
        try paths.verifyDirectTarget(quarantine, parent: parent)
        try fileManager.moveItem(at: target, to: quarantine)
        guard try entryIdentity(quarantine) == original else { throw InstallError.unsafePath }
        try paths.revalidateRoot(rootIdentity)
        guard try entryIdentity(quarantine) == original else { throw InstallError.unsafePath }
        try fileManager.removeItem(at: quarantine)
    }

    func writeTransaction(_ transaction: InstallTransaction, paths: InstallPaths, rootIdentity: FileIdentity) throws {
        try paths.revalidateRoot(rootIdentity)
        try paths.verifyDirectTarget(paths.transactionURL, parent: paths.appDirectoryURL)
        let data = try JSONEncoder().encode(transaction)
        try data.write(to: paths.transactionURL, options: [.atomic])
    }

    func readTransaction(paths: InstallPaths, rootIdentity: FileIdentity) throws -> InstallTransaction? {
        try paths.revalidateRoot(rootIdentity)
        guard exists(paths.transactionURL) else { return nil }
        try paths.verifyDirectTarget(paths.transactionURL, parent: paths.appDirectoryURL)
        return try JSONDecoder().decode(InstallTransaction.self, from: Data(contentsOf: paths.transactionURL))
    }

    private func entryIdentity(_ url: URL) throws -> FileIdentity {
        var info = stat()
        guard lstat(url.path, &info) == 0 else { throw InstallError.unsafePath }
        return FileIdentity(device: UInt64(info.st_dev), inode: UInt64(info.st_ino), owner: info.st_uid)
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
final class InstallCoordinator: InstallCoordinating {
    private let paths: InstallPaths
    private let validator: any BundleValidating
    private let service: any ServiceControlling
    private let health: any HealthChecking
    private let relauncher: any Relaunching
    private let fileSystem: any InstallFileOperating

    init(
        paths: InstallPaths,
        validator: any BundleValidating,
        service: any ServiceControlling,
        health: any HealthChecking,
        relauncher: any Relaunching,
        fileSystem: any InstallFileOperating = LocalInstallFileSystem()
    ) {
        self.paths = paths
        self.validator = validator
        self.service = service
        self.health = health
        self.relauncher = relauncher
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
        let rootIdentity = try paths.prepareInstallLayout()
        let staging = try paths.stagingBundleURL()
        let installedExists = fileSystem.exists(paths.installedBundleURL)
        guard !fileSystem.exists(staging) else { throw InstallError.unsafePath }

        onState(.copying)
        do { try fileSystem.copyItem(at: sourceBundle, to: staging) }
        catch { throw InstallError.copyFailed }
        defer {
            if fileSystem.exists(staging) {
                try? fileSystem.quarantineAndDelete(staging, parent: paths.appDirectoryURL, paths: paths, rootIdentity: rootIdentity)
            }
        }

        onState(.validating)
        let validated = try validator.validate(candidate: staging, replacing: installedExists ? paths.installedBundleURL : nil)
        let transaction = InstallTransaction(
            previousVersion: validated.previousVersion,
            candidateVersion: validated.version,
            priorServiceState: service.status()
        )
        try fileSystem.writeTransaction(transaction, paths: paths, rootIdentity: rootIdentity)

        do { try await service.stopAndUnregister() }
        catch { throw InstallError.serviceRegistrationFailed }

        var previousBundleMovedToRollback = false
        do {
            if fileSystem.exists(paths.rollbackBundleURL) {
                try fileSystem.quarantineAndDelete(paths.rollbackBundleURL, parent: paths.appDirectoryURL, paths: paths, rootIdentity: rootIdentity)
            }
            if installedExists {
                try fileSystem.moveItem(at: paths.installedBundleURL, to: paths.rollbackBundleURL, paths: paths, rootIdentity: rootIdentity)
                previousBundleMovedToRollback = true
            }
            try fileSystem.moveItem(at: staging, to: paths.installedBundleURL, paths: paths, rootIdentity: rootIdentity)
        } catch {
            throw await recover(
                primary: .activation,
                transaction: transaction,
                rootIdentity: rootIdentity,
                previousBundleMovedToRollback: previousBundleMovedToRollback
            )
        }

        do {
            try relauncher.relaunch(executable: paths.installedExecutableURL, arguments: ["--setup-installed"])
        } catch {
            throw await recover(primary: .relaunch, transaction: transaction, rootIdentity: rootIdentity)
        }
        return .relaunchRequired(previousVersion: validated.previousVersion, installedVersion: validated.version)
    }

    func finishInstalledSetup(
        onState: @escaping @MainActor (InstallerState) -> Void = { _ in }
    ) async throws -> InstallResult {
        let rootIdentity = try paths.prepareInstallLayout()
        let transaction = try fileSystem.readTransaction(paths: paths, rootIdentity: rootIdentity)
        let installedVersion = try validator.version(at: paths.installedBundleURL)
        let attemptStartedAt = Date()
        onState(.starting)
        do {
            try await service.registerAndWaitForApproval { onState(.waitingApproval) }
        } catch {
            if let transaction {
                throw await recover(primary: .registration, transaction: transaction, rootIdentity: rootIdentity)
            }
            throw (error as? InstallError) ?? InstallError.serviceRegistrationFailed
        }

        let healthy: Bool
        do {
            healthy = try await health.waitForHealthy(
                paths: paths,
                expectedVersion: installedVersion,
                notBefore: attemptStartedAt,
                timeout: .seconds(5)
            )
        } catch {
            if let transaction {
                throw await recover(primary: .health, transaction: transaction, rootIdentity: rootIdentity)
            }
            throw error
        }
        guard healthy else {
            if let transaction {
                throw await recover(primary: .health, transaction: transaction, rootIdentity: rootIdentity)
            }
            throw InstallError.healthCheckFailed
        }

        if fileSystem.exists(paths.rollbackBundleURL) {
            try? fileSystem.quarantineAndDelete(paths.rollbackBundleURL, parent: paths.appDirectoryURL, paths: paths, rootIdentity: rootIdentity)
        }
        if fileSystem.exists(paths.transactionURL) {
            try? fileSystem.quarantineAndDelete(paths.transactionURL, parent: paths.appDirectoryURL, paths: paths, rootIdentity: rootIdentity)
        }
        onState(.success)
        return .success(version: installedVersion)
    }

    private func recover(
        primary: InstallFailure,
        transaction: InstallTransaction,
        rootIdentity: FileIdentity,
        previousBundleMovedToRollback: Bool = true
    ) async -> InstallError {
        do {
            try await service.stopAndUnregister()
            if let previousVersion = transaction.previousVersion {
                if previousBundleMovedToRollback {
                    guard fileSystem.exists(paths.rollbackBundleURL) else {
                        return .transactionFailed(primary: primary, recovery: .failed)
                    }
                    if fileSystem.exists(paths.installedBundleURL) {
                        try fileSystem.quarantineAndDelete(paths.installedBundleURL, parent: paths.appDirectoryURL, paths: paths, rootIdentity: rootIdentity)
                    }
                    try fileSystem.moveItem(at: paths.rollbackBundleURL, to: paths.installedBundleURL, paths: paths, rootIdentity: rootIdentity)
                } else {
                    guard fileSystem.exists(paths.installedBundleURL),
                          (try? validator.version(at: paths.installedBundleURL)) == previousVersion
                    else { return .transactionFailed(primary: primary, recovery: .failed) }
                }
                if transaction.priorServiceState == .enabled || transaction.priorServiceState == .requiresApproval {
                    let attempt = Date()
                    try relauncher.relaunch(
                        executable: paths.installedExecutableURL,
                        arguments: ["--setup-installed", "--rollback-recovered"]
                    )
                    let healthy = try await health.waitForHealthy(
                        paths: paths,
                        expectedVersion: previousVersion,
                        notBefore: attempt,
                        timeout: .seconds(5)
                    )
                    guard healthy else { return .transactionFailed(primary: primary, recovery: .failed) }
                    return .transactionFailed(primary: primary, recovery: .restoredAndRelaunched)
                }
                return .transactionFailed(primary: primary, recovery: .restoredInactive)
            }

            if fileSystem.exists(paths.installedBundleURL) {
                try fileSystem.quarantineAndDelete(paths.installedBundleURL, parent: paths.appDirectoryURL, paths: paths, rootIdentity: rootIdentity)
            }
            if fileSystem.exists(paths.runURL) {
                try fileSystem.quarantineAndDelete(paths.runURL, parent: paths.rootURL, paths: paths, rootIdentity: rootIdentity)
            }
            return .transactionFailed(primary: primary, recovery: .firstInstallRemoved)
        } catch {
            return .transactionFailed(primary: primary, recovery: .failed)
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
