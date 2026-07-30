import Foundation

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
    case healthCheckFailedRolledBack
    case uninstallConfirmationRequired
}

extension InstallError: LocalizedError {
    var errorDescription: String? {
        switch self {
        case .unsafePath: "The install path is unsafe. No files were changed."
        case .invalidBundle: "The app bundle failed validation. Download or build a fresh signed copy."
        case let .downgradeRejected(installed, candidate):
            "Downgrade blocked: installed \(installed), candidate \(candidate). Use a newer build."
        case .copyFailed: "The app could not be staged. Check available disk space and try again."
        case .activationFailed: "The staged app could not be activated. The previous app was preserved when available."
        case .relaunchFailed: "The installed app could not be opened. Open it from the Application Support install path."
        case .serviceRegistrationFailed: "The background service could not be registered. Reopen the installed app and try again."
        case .approvalTimedOut: "Background-item approval was not completed in time. Approve it in System Settings, then retry."
        case .healthCheckFailed: "The local runtime did not become healthy. Reopen the installed app to retry."
        case .healthCheckFailedRolledBack: "The update failed its health check, so the previous version was restored."
        case .uninstallConfirmationRequired: "Complete uninstall cancelled because the confirmation token did not match."
        }
    }

    var recoveryAction: String {
        switch self {
        case .approvalTimedOut, .serviceRegistrationFailed:
            "Open System Settings > General > Login Items and allow Personal Computer Agent, then retry."
        case .healthCheckFailedRolledBack:
            "Continue using the restored version and build a corrected update."
        case .uninstallConfirmationRequired:
            "Run the command again and enter the exact confirmation token if data deletion is intended."
        default:
            "Retry with a fresh signed installer. Persistent data has not been deleted."
        }
    }
}

struct InstallPaths: Equatable, Sendable {
    let rootURL: URL
    let appDirectoryURL: URL
    let dataURL: URL
    let runURL: URL
    let installedBundleURL: URL
    let rollbackBundleURL: URL

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
        guard rootURL.isFileURL,
              rootURL.path.hasPrefix("/"),
              rootURL.path != "/",
              !rootURL.pathComponents.contains("..")
        else { throw InstallError.unsafePath }

        let canonicalRoot = rootURL.standardizedFileURL.resolvingSymlinksInPath()
        guard canonicalRoot.path != "/" else { throw InstallError.unsafePath }
        self.rootURL = canonicalRoot
        appDirectoryURL = canonicalRoot.appendingPathComponent("App", isDirectory: true)
        dataURL = canonicalRoot.appendingPathComponent("Data", isDirectory: true)
        runURL = canonicalRoot.appendingPathComponent("Run", isDirectory: true)
        installedBundleURL = appDirectoryURL.appendingPathComponent("PersonalComputerAgent.app", isDirectory: true)
        rollbackBundleURL = appDirectoryURL.appendingPathComponent(".rollback", isDirectory: true)
        try Self.requireDirectChild(appDirectoryURL, of: canonicalRoot)
        try Self.requireDirectChild(dataURL, of: canonicalRoot)
        try Self.requireDirectChild(runURL, of: canonicalRoot)
        try Self.requireDirectChild(installedBundleURL, of: appDirectoryURL)
        try Self.requireDirectChild(rollbackBundleURL, of: appDirectoryURL)
    }

    func createDirectories(fileManager: FileManager = .default) throws {
        for directory in [rootURL, appDirectoryURL, dataURL, runURL] {
            try fileManager.createDirectory(
                at: directory,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            try fileManager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
        }
    }

    func stagingBundleURL(identifier: UUID = UUID()) throws -> URL {
        let url = appDirectoryURL.appendingPathComponent(".staging-\(identifier.uuidString)", isDirectory: true)
        try Self.requireDirectChild(url, of: appDirectoryURL)
        return url
    }

    func verifyDeletionTarget(_ target: URL, directChildOf parent: URL) throws {
        try Self.requireDirectChild(
            target.standardizedFileURL.resolvingSymlinksInPath(),
            of: parent.standardizedFileURL.resolvingSymlinksInPath()
        )
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

@MainActor
protocol ServiceControlling: AnyObject {
    func stopAndUnregister() async throws
    func registerAndWaitForApproval(onWaitingForApproval: @escaping @MainActor () -> Void) async throws
}

@MainActor
protocol HealthChecking: AnyObject {
    func waitForHealthy(paths: InstallPaths, timeout: Duration) async throws -> Bool
}

@MainActor
protocol Relaunching: AnyObject {
    func relaunch(executable: URL, arguments: [String]) throws
}

enum InstallResult: Equatable, Sendable {
    case relaunchRequired(previousVersion: String?, installedVersion: String)
    case success(version: String)
}

@MainActor
final class InstallCoordinator {
    private let paths: InstallPaths
    private let validator: any BundleValidating
    private let service: any ServiceControlling
    private let health: any HealthChecking
    private let relauncher: any Relaunching
    private let fileManager: FileManager

    init(
        paths: InstallPaths,
        validator: any BundleValidating,
        service: any ServiceControlling,
        health: any HealthChecking,
        relauncher: any Relaunching,
        fileManager: FileManager = .default
    ) {
        self.paths = paths
        self.validator = validator
        self.service = service
        self.health = health
        self.relauncher = relauncher
        self.fileManager = fileManager
    }

    func installOrFinish(
        from sourceBundle: URL,
        onState: @escaping @MainActor (InstallerState) -> Void = { _ in }
    ) async throws -> InstallResult {
        let source = sourceBundle.standardizedFileURL.resolvingSymlinksInPath()
        let installed = paths.installedBundleURL.standardizedFileURL.resolvingSymlinksInPath()
        if source == installed {
            return try await finishInstalledSetup(onState: onState)
        }
        return try await prepareInstallation(from: sourceBundle, onState: onState)
    }

    func prepareInstallation(
        from sourceBundle: URL,
        onState: @escaping @MainActor (InstallerState) -> Void = { _ in }
    ) async throws -> InstallResult {
        try paths.createDirectories(fileManager: fileManager)
        let staging = try paths.stagingBundleURL()
        let installedExists = fileManager.fileExists(atPath: paths.installedBundleURL.path)
        let rollbackExists = fileManager.fileExists(atPath: paths.rollbackBundleURL.path)

        if fileManager.fileExists(atPath: staging.path) {
            try remove(staging, directChildOf: paths.appDirectoryURL)
        }
        onState(.copying)
        do {
            try fileManager.copyItem(at: sourceBundle, to: staging)
        } catch {
            throw InstallError.copyFailed
        }
        defer {
            if fileManager.fileExists(atPath: staging.path) {
                try? remove(staging, directChildOf: paths.appDirectoryURL)
            }
        }

        onState(.validating)
        let validated = try validator.validate(
            candidate: staging,
            replacing: installedExists ? paths.installedBundleURL : nil
        )

        try await service.stopAndUnregister()
        if rollbackExists {
            try remove(paths.rollbackBundleURL, directChildOf: paths.appDirectoryURL)
        }
        if installedExists {
            do {
                try fileManager.moveItem(at: paths.installedBundleURL, to: paths.rollbackBundleURL)
            } catch {
                throw InstallError.activationFailed
            }
        }
        do {
            try fileManager.moveItem(at: staging, to: paths.installedBundleURL)
        } catch {
            if fileManager.fileExists(atPath: paths.rollbackBundleURL.path) {
                try? fileManager.moveItem(at: paths.rollbackBundleURL, to: paths.installedBundleURL)
            }
            throw InstallError.activationFailed
        }

        do {
            try relauncher.relaunch(executable: paths.installedExecutableURL, arguments: ["--setup-installed"])
        } catch {
            try? rollbackActivatedBundle()
            throw InstallError.relaunchFailed
        }
        return .relaunchRequired(
            previousVersion: validated.previousVersion,
            installedVersion: validated.version
        )
    }

    func finishInstalledSetup(
        onState: @escaping @MainActor (InstallerState) -> Void = { _ in }
    ) async throws -> InstallResult {
        let installedVersion = try validator.version(at: paths.installedBundleURL)
        onState(.starting)
        do {
            try await service.registerAndWaitForApproval {
                onState(.waitingApproval)
            }
        } catch let error as InstallError {
            throw error
        } catch {
            throw InstallError.serviceRegistrationFailed
        }

        let healthy = try await health.waitForHealthy(paths: paths, timeout: .seconds(5))
        guard healthy else {
            if fileManager.fileExists(atPath: paths.rollbackBundleURL.path) {
                try await service.stopAndUnregister()
                try rollbackActivatedBundle()
                try relauncher.relaunch(
                    executable: paths.installedExecutableURL,
                    arguments: ["--setup-installed", "--rollback-recovered"]
                )
                throw InstallError.healthCheckFailedRolledBack
            }
            throw InstallError.healthCheckFailed
        }

        if fileManager.fileExists(atPath: paths.rollbackBundleURL.path) {
            try remove(paths.rollbackBundleURL, directChildOf: paths.appDirectoryURL)
        }
        onState(.success)
        return .success(version: installedVersion)
    }

    private func rollbackActivatedBundle() throws {
        guard fileManager.fileExists(atPath: paths.rollbackBundleURL.path) else { return }
        if fileManager.fileExists(atPath: paths.installedBundleURL.path) {
            try remove(paths.installedBundleURL, directChildOf: paths.appDirectoryURL)
        }
        do {
            try fileManager.moveItem(at: paths.rollbackBundleURL, to: paths.installedBundleURL)
        } catch {
            throw InstallError.activationFailed
        }
    }

    private func remove(_ url: URL, directChildOf parent: URL) throws {
        try paths.verifyDeletionTarget(url, directChildOf: parent)
        try fileManager.removeItem(at: url)
    }
}

struct Version: Equatable, Sendable {
    private let components: [Int]

    var isValid: Bool {
        components.count == 3 && components.allSatisfy { $0 >= 0 }
    }

    init(_ raw: String) {
        components = raw.split(separator: ".").map { Int($0) ?? -1 }
    }

    func compare(to other: Version) -> ComparisonResult {
        let count = max(components.count, other.components.count)
        for index in 0..<count {
            let lhs = index < components.count ? components[index] : 0
            let rhs = index < other.components.count ? other.components[index] : 0
            if lhs < rhs { return .orderedAscending }
            if lhs > rhs { return .orderedDescending }
        }
        return .orderedSame
    }
}
