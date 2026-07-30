import Darwin
import Foundation
import XCTest
@testable import PersonalComputerAgent

@MainActor
final class InstallCoordinatorTests: XCTestCase {
    func testFirstInstallStagesBundlePreservesDataAndRelaunchesInstalledExecutable() async throws {
        let fixture = try Fixture(installedVersion: nil, candidateVersion: "1.0.0")
        try FileManager.default.createDirectory(at: fixture.paths.dataURL, withIntermediateDirectories: false)
        try Data("keep".utf8).write(to: fixture.paths.dataURL.appendingPathComponent("fact"))

        let result = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)

        XCTAssertEqual(result, .relaunchRequired(previousVersion: nil, installedVersion: "1.0.0"))
        XCTAssertTrue(FileManager.default.fileExists(atPath: fixture.paths.installedBundleURL.path))
        XCTAssertEqual(try Data(contentsOf: fixture.paths.dataURL.appendingPathComponent("fact")), Data("keep".utf8))
        XCTAssertEqual(fixture.relauncher.urls, [fixture.paths.installedExecutableURL])
        XCTAssertEqual(fixture.service.stopCount, 1)
    }

    func testInstalledCopyFinishesRegistrationWithoutReplacingItself() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "1.0.0")

        let result = try await fixture.coordinator.installOrFinish(from: fixture.paths.installedBundleURL)

        XCTAssertEqual(result, .success(version: "1.0.0"))
        XCTAssertEqual(fixture.service.registerCount, 1)
        XCTAssertEqual(fixture.health.checkCount, 1)
        XCTAssertTrue(fixture.relauncher.urls.isEmpty)
    }

    func testRepeatInstallOfSameVersionUsesReplacementFlow() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "1.0.0")

        let result = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)

        XCTAssertEqual(result, .relaunchRequired(previousVersion: "1.0.0", installedVersion: "1.0.0"))
        XCTAssertTrue(FileManager.default.fileExists(atPath: fixture.paths.rollbackBundleURL.path))
        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
    }

    func testDowngradeIsRejectedBeforeStoppingRunningService() async throws {
        let fixture = try Fixture(installedVersion: "2.0.0", candidateVersion: "1.9.9")

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.prepareInstallation(from: fixture.candidate)) { error in
            XCTAssertEqual(error as? InstallError, .downgradeRejected(installed: "2.0.0", candidate: "1.9.9"))
        }
        XCTAssertEqual(fixture.service.stopCount, 0)
        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "2.0.0")
    }

    func testStopFailureClearsPreparedJournalWithoutChangingOldBundleOrService() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        fixture.service.currentState = .enabled
        fixture.service.stopError = .serviceRegistrationFailed

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.prepareInstallation(from: fixture.candidate)) { error in
            XCTAssertEqual(error as? InstallError, .serviceRegistrationFailed)
        }

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(fixture.service.currentState, .enabled)
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.transactionURL))
        XCTAssertFalse(try FileManager.default.contentsOfDirectory(atPath: fixture.paths.appDirectoryURL.path)
            .contains(where: { $0.hasPrefix(".staging-") }))
    }

    func testFailedUpgradeHealthRestoresOldBundleWithoutDeletingData() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        fixture.service.currentState = .enabled
        try FileManager.default.createDirectory(at: fixture.paths.dataURL, withIntermediateDirectories: false)
        try Data("durable".utf8).write(to: fixture.paths.dataURL.appendingPathComponent("fact"))
        _ = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)
        fixture.health.results = [false, true]

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.finishInstalledSetup()) { error in
            XCTAssertEqual(
                error as? InstallError,
                .transactionFailed(primary: .health, recovery: .restoredAndRelaunched)
            )
        }
        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(try Data(contentsOf: fixture.paths.dataURL.appendingPathComponent("fact")), Data("durable".utf8))
        XCTAssertEqual(fixture.relauncher.urls.last, fixture.paths.installedExecutableURL)
    }

    func testSuccessfulUpgradeRemovesRollbackOnlyAfterHealth() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        _ = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)

        let result = try await fixture.coordinator.finishInstalledSetup()

        XCTAssertEqual(result, .success(version: "2.0.0"))
        XCTAssertFalse(FileManager.default.fileExists(atPath: fixture.paths.rollbackBundleURL.path))
        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "2.0.0")
    }

    func testRootContainingTraversalIsRejected() throws {
        let parent = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let unsafe = URL(fileURLWithPath: parent.path + "/child/../escape", isDirectory: true)

        XCTAssertThrowsError(try InstallPaths(rootURL: unsafe)) { error in
            XCTAssertEqual(error as? InstallError, .unsafePath)
        }
    }

    func testRelaunchFailureRestoresPreviousBundleAndRelaunchesOldSetup() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        fixture.service.currentState = .enabled
        fixture.relauncher.failuresRemaining = 1

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.prepareInstallation(from: fixture.candidate)) { error in
            guard case .transactionFailed(primary: .relaunch, recovery: .restoredAndRelaunched) = error as? InstallError else {
                return XCTFail("unexpected error: \(error)")
            }
        }

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(fixture.relauncher.arguments.last, ["--setup-installed", "--rollback-recovered"])
        XCTAssertEqual(fixture.health.expectedVersions.last, "1.0.0")
    }

    func testRegistrationFailureUsesRollbackFunnelAndRestoresOldService() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        fixture.service.currentState = .enabled
        _ = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)
        fixture.service.registrationError = .serviceRegistrationFailed

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.finishInstalledSetup()) { error in
            guard case .transactionFailed(primary: .registration, recovery: .restoredAndRelaunched) = error as? InstallError else {
                return XCTFail("unexpected error: \(error)")
            }
        }

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertGreaterThanOrEqual(fixture.service.stopCount, 2)
        XCTAssertEqual(fixture.health.expectedVersions.last, "1.0.0")
    }

    func testFirstInstallHealthFailureRemovesAppAndRunButPreservesData() async throws {
        let fixture = try Fixture(installedVersion: nil, candidateVersion: "2.0.0")
        try FileManager.default.createDirectory(at: fixture.paths.dataURL, withIntermediateDirectories: false)
        let durable = fixture.paths.dataURL.appendingPathComponent("fact")
        try Data("durable".utf8).write(to: durable)
        _ = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)
        fixture.health.results = [false]

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.finishInstalledSetup()) { error in
            guard case .transactionFailed(primary: .health, recovery: .firstInstallRemoved) = error as? InstallError else {
                return XCTFail("unexpected error: \(error)")
            }
        }

        XCTAssertFalse(FileManager.default.fileExists(atPath: fixture.paths.installedBundleURL.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: fixture.paths.runURL.path))
        XCTAssertEqual(try Data(contentsOf: durable), Data("durable".utf8))
    }

    func testActivationFailureRestoresOldBundleThroughSingleRollbackFunnel() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0", failActivation: true)
        fixture.service.currentState = .enabled

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.prepareInstallation(from: fixture.candidate)) { error in
            guard case .transactionFailed(primary: .activation, recovery: .restoredAndRelaunched) = error as? InstallError else {
                return XCTFail("unexpected error: \(error)")
            }
        }

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(fixture.relauncher.arguments.last, ["--setup-installed", "--rollback-recovered"])
    }

    func testInstallLayoutDoesNotCreateOrChmodData() throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        let paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))

        _ = try paths.prepareInstallLayout()

        XCTAssertFalse(FileManager.default.fileExists(atPath: paths.dataURL.path))
    }

    func testExistingSymlinkRootAndDirectAppSymlinkAreRejectedWithoutFollowingThem() throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        let outside = temporary.appendingPathComponent("outside", isDirectory: true)
        try FileManager.default.createDirectory(at: outside, withIntermediateDirectories: false)
        let rootLink = temporary.appendingPathComponent("root-link", isDirectory: true)
        try FileManager.default.createSymbolicLink(at: rootLink, withDestinationURL: outside)
        XCTAssertThrowsError(try InstallPaths(rootURL: rootLink).prepareInstallLayout())

        let paths = try InstallPaths(rootURL: temporary.appendingPathComponent("real-root", isDirectory: true))
        _ = try paths.prepareInstallLayout()
        try FileManager.default.removeItem(at: paths.appDirectoryURL)
        try FileManager.default.createSymbolicLink(at: paths.appDirectoryURL, withDestinationURL: outside)
        XCTAssertThrowsError(try paths.prepareInstallLayout())
    }

    func testPhaseJournalAdvancesAtEveryDurableBoundaryAndCommitsBeforeCleanup() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")

        _ = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)
        XCTAssertEqual(fixture.fileSystem.recordedPhases, [.prepared, .oldMoved, .newActivated])

        _ = try await fixture.coordinator.finishInstalledSetup()
        XCTAssertEqual(fixture.fileSystem.recordedPhases, [.prepared, .oldMoved, .newActivated, .committed])
    }

    func testPreparedCrashRecoveryOnlyCleansStagingAndJournal() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        fixture.service.currentState = .enabled
        try fixture.simulateCrash(phase: .prepared)

        _ = try await fixture.coordinator.recoverPendingInstallation()

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(fixture.service.stopCount, 0)
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.transactionURL))
        XCTAssertFalse(fixture.fileSystem.exists(fixture.crashStagingURL))
    }

    func testPreparedCrashCarriesOriginalActiveStateIntoImmediateRetryJournal() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        try fixture.simulateCrash(phase: .prepared, priorServiceState: .enabled)
        fixture.service.currentState = .notRegistered

        _ = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)

        let transaction = try fixture.fileSystem.readTransaction(
            paths: fixture.paths,
            layoutIdentity: fixture.layoutIdentity
        )
        XCTAssertEqual(transaction?.phase, .newActivated)
        XCTAssertEqual(transaction?.priorServiceState, .enabled)
    }

    func testOldMovedCrashRecoveryRestoresOldActiveServiceAndIsIdempotent() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        try fixture.simulateCrash(phase: .oldMoved, priorServiceState: .enabled)

        _ = try await fixture.coordinator.recoverPendingInstallation()
        _ = try await fixture.coordinator.recoverPendingInstallation()

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(fixture.service.currentState, .enabled)
        XCTAssertEqual(fixture.health.expectedVersions, ["1.0.0"])
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.transactionURL))
    }

    func testNewActivatedCrashRecoveryRestoresInactiveOldVersionWithoutStartingIt() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        try fixture.simulateCrash(phase: .newActivated, priorServiceState: .notRegistered)

        _ = try await fixture.coordinator.recoverPendingInstallation()

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(fixture.service.currentState, .notRegistered)
        XCTAssertEqual(fixture.service.registerCount, 0)
        XCTAssertEqual(fixture.health.checkCount, 0)
    }

    func testNewActivatedFirstInstallCrashRemovesOnlyAppAndRun() async throws {
        let fixture = try Fixture(installedVersion: nil, candidateVersion: "2.0.0")
        try FileManager.default.createDirectory(at: fixture.paths.dataURL, withIntermediateDirectories: false)
        let marker = fixture.paths.dataURL.appendingPathComponent("keep")
        try Data("keep".utf8).write(to: marker)
        try fixture.simulateCrash(phase: .newActivated, previousVersion: nil)

        _ = try await fixture.coordinator.recoverPendingInstallation()

        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.installedBundleURL))
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.runURL))
        XCTAssertEqual(try Data(contentsOf: marker), Data("keep".utf8))
    }

    func testCommittedCrashNeverRollsBackAndCleanupFailureRemainsRetryable() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        try fixture.simulateCrash(phase: .committed)
        fixture.fileSystem.failNextCleanup = true

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.recoverPendingInstallation()) { error in
            XCTAssertEqual(error as? InstallError, .committedCleanupFailed)
        }
        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "2.0.0")
        XCTAssertEqual(try fixture.fileSystem.readTransaction(paths: fixture.paths, layoutIdentity: fixture.layoutIdentity)?.phase, .committed)

        _ = try await fixture.coordinator.recoverPendingInstallation()
        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "2.0.0")
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.rollbackBundleURL))
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.transactionURL))
    }

    func testRolledBackCrashOnlyFinishesCleanupWithoutRestartingService() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        try fixture.simulateCrash(phase: .rolledBack)
        fixture.service.currentState = .enabled

        _ = try await fixture.coordinator.recoverPendingInstallation()

        XCTAssertEqual(try fixture.version(at: fixture.paths.installedBundleURL), "1.0.0")
        XCTAssertEqual(fixture.service.stopCount, 0)
        XCTAssertEqual(fixture.service.registerCount, 0)
        XCTAssertFalse(fixture.fileSystem.exists(fixture.crashStagingURL))
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.transactionURL))
    }

    func testRollbackCleanupFailureReportsRelaunchedCleanupPendingAndRetriesIdempotently() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        try fixture.simulateCrash(phase: .newActivated, priorServiceState: .enabled)
        fixture.fileSystem.failDeletionTarget = fixture.paths.transactionURL.lastPathComponent

        let recovery = try await fixture.coordinator.recoverPendingInstallation()

        XCTAssertEqual(recovery, .restoredAndRelaunchedCleanupPending)
        XCTAssertEqual(
            try fixture.fileSystem.readTransaction(paths: fixture.paths, layoutIdentity: fixture.layoutIdentity)?.phase,
            .rolledBack
        )
        fixture.fileSystem.failDeletionTarget = nil
        _ = try await fixture.coordinator.recoverPendingInstallation()
        XCTAssertEqual(fixture.service.registerCount, 1)
        XCTAssertFalse(fixture.fileSystem.exists(fixture.paths.transactionURL))
    }

    func testAppDirectoryReplacementBeforeStagingCopyFailsClosed() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        fixture.fileSystem.replaceAppBeforeCopy = true

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.prepareInstallation(from: fixture.candidate)) { error in
            XCTAssertEqual(error as? InstallError, .unsafePath)
        }
    }

    func testAppDirectoryReplacementBeforeActivationFailsClosed() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        fixture.fileSystem.replaceAppBeforeActivation = true

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.prepareInstallation(from: fixture.candidate)) { error in
            guard case .transactionFailed(primary: .activation, recovery: .failed) = error as? InstallError else {
                return XCTFail("unexpected error: \(error)")
            }
        }
    }
}

@MainActor
private final class Fixture {
    let temporary: URL
    let paths: InstallPaths
    let candidate: URL
    let service = FakeServiceController()
    let health = FakeHealthChecker()
    let relauncher = FakeRelauncher()
    let fileSystem: FaultingInstallFileSystem
    let coordinator: InstallCoordinator
    let layoutIdentity: InstallLayoutIdentity

    var crashStagingURL: URL { try! paths.stagingBundleURL(identifier: UUID(uuidString: "00000000-0000-0000-0000-000000000001")!) }

    init(installedVersion: String?, candidateVersion: String, failActivation: Bool = false) throws {
        temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))
        layoutIdentity = try paths.prepareInstallLayout()
        candidate = temporary.appendingPathComponent("candidate.app", isDirectory: true)
        try Self.makeBundle(at: candidate, version: candidateVersion)
        if let installedVersion {
            try Self.makeBundle(at: paths.installedBundleURL, version: installedVersion)
        }
        fileSystem = FaultingInstallFileSystem(failActivation: failActivation)
        coordinator = InstallCoordinator(
            paths: paths,
            validator: TestBundleValidator(),
            service: service,
            health: health,
            relauncher: relauncher,
            fileSystem: fileSystem
        )
    }

    deinit { try? FileManager.default.removeItem(at: temporary) }

    func version(at bundle: URL) throws -> String {
        try String(contentsOf: bundle.appendingPathComponent("version"), encoding: .utf8)
    }

    func simulateCrash(
        phase: InstallPhase,
        previousVersion: String? = "1.0.0",
        priorServiceState: ServiceState = .enabled
    ) throws {
        if fileSystem.exists(crashStagingURL) {
            try fileSystem.quarantineAndDelete(crashStagingURL, parent: paths.appDirectoryURL, paths: paths, layoutIdentity: layoutIdentity)
        }
        try Self.makeBundle(at: crashStagingURL, version: "2.0.0")
        if phase == .oldMoved || phase == .newActivated || phase == .committed {
            if let previousVersion, fileSystem.exists(paths.installedBundleURL) {
                XCTAssertEqual(try version(at: paths.installedBundleURL), previousVersion)
                try fileSystem.moveItem(at: paths.installedBundleURL, to: paths.rollbackBundleURL, paths: paths, layoutIdentity: layoutIdentity)
            }
        }
        if phase == .newActivated || phase == .committed {
            try fileSystem.moveItem(at: crashStagingURL, to: paths.installedBundleURL, paths: paths, layoutIdentity: layoutIdentity)
        }
        try fileSystem.writeTransaction(
            InstallTransaction(
                phase: phase,
                previousVersion: previousVersion,
                candidateVersion: "2.0.0",
                priorServiceState: priorServiceState,
                stagingName: crashStagingURL.lastPathComponent,
                expectedLayoutIdentity: layoutIdentity
            ),
            paths: paths,
            layoutIdentity: layoutIdentity
        )
        fileSystem.recordedPhases.removeAll()
        service.currentState = phase == .prepared ? priorServiceState : .notRegistered
    }

    private static func makeBundle(at url: URL, version: String) throws {
        try FileManager.default.createDirectory(
            at: url.appendingPathComponent("Contents/MacOS", isDirectory: true),
            withIntermediateDirectories: true
        )
        try Data(version.utf8).write(to: url.appendingPathComponent("version"))
        try Data("executable".utf8).write(to: url.appendingPathComponent("Contents/MacOS/PersonalComputerAgent"))
    }
}

private struct TestBundleValidator: BundleValidating {
    func validate(candidate: URL, replacing installed: URL?) throws -> ValidatedBundle {
        let candidateVersion = try version(at: candidate)
        let installedVersion = try installed.map(version(at:))
        if let installedVersion,
           Version(candidateVersion).compare(to: Version(installedVersion)) == .orderedAscending {
            throw InstallError.downgradeRejected(installed: installedVersion, candidate: candidateVersion)
        }
        return ValidatedBundle(version: candidateVersion, previousVersion: installedVersion)
    }

    func version(at bundle: URL) throws -> String { try String(contentsOf: bundle.appendingPathComponent("version"), encoding: .utf8) }
}

@MainActor
private final class FakeServiceController: ServiceControlling {
    var currentState: ServiceState = .notRegistered
    var registrationError: InstallError?
    var stopError: InstallError?
    var stopCount = 0
    var registerCount = 0
    func status() -> ServiceState { currentState }
    func stopAndUnregister() async throws {
        stopCount += 1
        if let stopError { throw stopError }
        currentState = .notRegistered
    }
    func registerAndWaitForApproval(onWaitingForApproval: @escaping @MainActor () -> Void) async throws {
        registerCount += 1
        if let registrationError {
            self.registrationError = nil
            throw registrationError
        }
        currentState = .enabled
    }
}

@MainActor
private final class FakeHealthChecker: HealthChecking {
    var isHealthy = true
    var results: [Bool] = []
    var checkCount = 0
    var expectedVersions: [String] = []
    func waitForHealthy(
        paths: InstallPaths,
        expectedVersion: String,
        notBefore: Date,
        timeout: Duration
    ) async throws -> Bool {
        checkCount += 1
        expectedVersions.append(expectedVersion)
        return results.isEmpty ? isHealthy : results.removeFirst()
    }
}

@MainActor
private final class FakeRelauncher: Relaunching {
    var urls: [URL] = []
    var arguments: [[String]] = []
    var failuresRemaining = 0
    func relaunch(executable: URL, arguments: [String]) throws {
        urls.append(executable)
        self.arguments.append(arguments)
        if failuresRemaining > 0 {
            failuresRemaining -= 1
            throw InstallError.relaunchFailed
        }
    }
}

private final class FaultingInstallFileSystem: InstallFileOperating {
    private let base = LocalInstallFileSystem()
    private var failActivation: Bool
    var recordedPhases: [InstallPhase] = []
    var failNextCleanup = false
    var failDeletionTarget: String?
    var replaceAppBeforeCopy = false
    var replaceAppBeforeActivation = false
    var replaceRootChildBeforeQuarantine = false

    init(failActivation: Bool) { self.failActivation = failActivation }
    func exists(_ url: URL) -> Bool { base.exists(url) }
    func identity(of url: URL) throws -> FileIdentity { try base.identity(of: url) }
    func copyItem(at source: URL, to destination: URL, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws {
        if replaceAppBeforeCopy {
            replaceAppBeforeCopy = false
            try replaceAppDirectory(paths)
        }
        try base.copyItem(at: source, to: destination, paths: paths, layoutIdentity: layoutIdentity)
    }
    func moveItem(at source: URL, to destination: URL, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws {
        if replaceAppBeforeActivation, destination == paths.installedBundleURL, source.lastPathComponent.hasPrefix(".staging-") {
            replaceAppBeforeActivation = false
            try replaceAppDirectory(paths)
        }
        if failActivation, destination == paths.installedBundleURL, source.lastPathComponent.hasPrefix(".staging-") {
            failActivation = false
            throw InstallError.activationFailed
        }
        try base.moveItem(at: source, to: destination, paths: paths, layoutIdentity: layoutIdentity)
    }
    func quarantineAndDelete(_ target: URL, parent: URL, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws {
        if target.lastPathComponent == failDeletionTarget {
            throw InstallError.unsafePath
        }
        if failNextCleanup {
            failNextCleanup = false
            throw InstallError.unsafePath
        }
        try base.quarantineAndDelete(target, parent: parent, paths: paths, layoutIdentity: layoutIdentity)
    }
    func quarantineRootChild(
        _ target: URL,
        parent: URL,
        paths: InstallPaths,
        rootIdentity: FileIdentity,
        expectedIdentity: FileIdentity
    ) throws {
        if replaceRootChildBeforeQuarantine, target == paths.appDirectoryURL {
            replaceRootChildBeforeQuarantine = false
            let original = paths.rootURL.appendingPathComponent("captured-App")
            try FileManager.default.moveItem(at: target, to: original)
            try FileManager.default.createDirectory(at: target, withIntermediateDirectories: false)
        }
        try base.quarantineRootChild(
            target,
            parent: parent,
            paths: paths,
            rootIdentity: rootIdentity,
            expectedIdentity: expectedIdentity
        )
    }
    func writeTransaction(_ transaction: InstallTransaction, paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws {
        recordedPhases.append(transaction.phase)
        try base.writeTransaction(transaction, paths: paths, layoutIdentity: layoutIdentity)
    }
    func readTransaction(paths: InstallPaths, layoutIdentity: InstallLayoutIdentity) throws -> InstallTransaction? {
        try base.readTransaction(paths: paths, layoutIdentity: layoutIdentity)
    }

    private func replaceAppDirectory(_ paths: InstallPaths) throws {
        let replaced = paths.rootURL.appendingPathComponent("replaced-App-\(UUID().uuidString)")
        try FileManager.default.moveItem(at: paths.appDirectoryURL, to: replaced)
        try FileManager.default.createDirectory(at: paths.appDirectoryURL, withIntermediateDirectories: false)
    }
}

@MainActor
final class RuntimeHealthCheckerTests: XCTestCase {
    func testAnyPreAttemptStatusIsRejectedWithoutClockTolerance() async throws {
        let fixture = try HealthFixture()
        let attempt = Date(timeIntervalSince1970: floor(Date().timeIntervalSince1970))
        fixture.processInspector.identity = fixture.validIdentity(startedAt: attempt)
        try fixture.writeStatus(version: "2.0.0", pid: 321, heartbeat: attempt.addingTimeInterval(-0.25))
        try FileManager.default.setAttributes(
            [.modificationDate: attempt.addingTimeInterval(-0.25)],
            ofItemAtPath: fixture.statusURL.path
        )

        let healthy = try await fixture.checker.waitForHealthy(
            paths: fixture.paths,
            expectedVersion: "2.0.0",
            notBefore: attempt,
            timeout: .milliseconds(15)
        )

        XCTAssertFalse(healthy)
    }

    func testWrongExecutableAndReusedOldProcessAreRejected() async throws {
        let fixture = try HealthFixture()
        let attempt = Date(timeIntervalSince1970: floor(Date().timeIntervalSince1970) - 1)
        try fixture.writeStatus(version: "2.0.0", pid: 321, heartbeat: attempt)
        try FileManager.default.setAttributes([.modificationDate: attempt], ofItemAtPath: fixture.statusURL.path)

        fixture.processInspector.identity = ProcessIdentity(
            userID: getuid(),
            executableURL: URL(fileURLWithPath: "/tmp/not-pca-agentd"),
            startedAt: attempt
        )
        let wrongExecutable = try await fixture.checker.waitForHealthy(
            paths: fixture.paths,
            expectedVersion: "2.0.0",
            notBefore: attempt,
            timeout: .milliseconds(10)
        )
        XCTAssertFalse(wrongExecutable)

        fixture.processInspector.identity = fixture.validIdentity(startedAt: attempt.addingTimeInterval(-1))
        let reusedOldProcess = try await fixture.checker.waitForHealthy(
            paths: fixture.paths,
            expectedVersion: "2.0.0",
            notBefore: attempt,
            timeout: .milliseconds(10)
        )
        XCTAssertFalse(reusedOldProcess)
    }

    func testExactFreshCandidateProcessIdentityIsAccepted() async throws {
        let fixture = try HealthFixture()
        let attempt = Date(timeIntervalSince1970: floor(Date().timeIntervalSince1970) - 1)
        fixture.processInspector.identity = fixture.validIdentity(startedAt: attempt)
        try fixture.writeStatus(version: "2.0.0", pid: 321, heartbeat: attempt)
        try FileManager.default.setAttributes([.modificationDate: attempt], ofItemAtPath: fixture.statusURL.path)

        let healthy = try await fixture.checker.waitForHealthy(
            paths: fixture.paths,
            expectedVersion: "2.0.0",
            notBefore: attempt,
            timeout: .milliseconds(20)
        )

        XCTAssertTrue(healthy)
    }
}

@MainActor
private final class HealthFixture {
    let temporary: URL
    let paths: InstallPaths
    let statusURL: URL
    let checker: RuntimeHealthChecker
    let processInspector = FakeProcessInspector()

    init() throws {
        temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))
        _ = try paths.prepareInstallLayout()
        statusURL = paths.runURL.appendingPathComponent("runtime-status.json")
        checker = RuntimeHealthChecker(
            pollInterval: .milliseconds(1),
            processInspector: processInspector
        )
    }

    deinit { try? FileManager.default.removeItem(at: temporary) }

    func writeStatus(version: String, pid: Int32, heartbeat: Date, schema: Int = 1) throws {
        let formatter = ISO8601DateFormatter()
        let object: [String: Any] = [
            "agent_status": "unpaired",
            "bridge_status": "degraded",
            "local_healthy": true,
            "heartbeat_at": formatter.string(from: heartbeat),
            "process_id": pid,
            "app_version": version,
            "schema_version": schema,
        ]
        try JSONSerialization.data(withJSONObject: object).write(to: statusURL, options: .atomic)
    }

    func validIdentity(startedAt: Date) -> ProcessIdentity {
        ProcessIdentity(
            userID: getuid(),
            executableURL: paths.installedBundleURL.appendingPathComponent("Contents/Resources/bin/pca-agentd"),
            startedAt: startedAt
        )
    }
}

private final class FakeProcessInspector: ProcessInspecting {
    var identity: ProcessIdentity?
    func inspect(processID: Int32) -> ProcessIdentity? { processID == 321 ? identity : nil }
}

@MainActor
final class ServiceControllerTests: XCTestCase {
    func testApprovalTimeoutIsBoundedAndOpensSystemSettingsOnce() async throws {
        let backend = FakeServiceBackend(status: .requiresApproval)
        let controller = ServiceController(
            backend: backend,
            approvalTimeout: .milliseconds(12),
            pollInterval: .milliseconds(2)
        )

        await XCTAssertThrowsErrorAsync(try await controller.registerAndWaitForApproval {}) { error in
            XCTAssertEqual(error as? InstallError, .approvalTimedOut)
        }
        XCTAssertEqual(backend.openSettingsCount, 1)
    }

    func testApprovalPollingIsCancellable() async throws {
        let backend = FakeServiceBackend(status: .requiresApproval)
        let controller = ServiceController(
            backend: backend,
            approvalTimeout: .seconds(1),
            pollInterval: .milliseconds(20)
        )
        let task = Task { try await controller.registerAndWaitForApproval {} }
        try await Task.sleep(for: .milliseconds(5))
        task.cancel()

        do {
            try await task.value
            XCTFail("expected cancellation")
        } catch is CancellationError {
            XCTAssertEqual(backend.openSettingsCount, 1)
        }
    }
}

@MainActor
private final class FakeServiceBackend: ServiceBackend {
    var state: ServiceState
    var openSettingsCount = 0
    init(status: ServiceState) { state = status }
    func status() -> ServiceState { state }
    func register() throws { state = .requiresApproval }
    func unregister() async throws { state = .notRegistered }
    func openSystemSettingsLoginItems() { openSettingsCount += 1 }
}

@MainActor
final class InstallerViewModelTests: XCTestCase {
    func testRollbackRelaunchTerminatesCurrentInstallerWithoutShowingConcurrentFailureUI() async {
        let terminator = FakeTerminator()
        let model = InstallerViewModel(
            coordinator: FailingInstallCoordinator(),
            sourceBundle: URL(fileURLWithPath: "/tmp/source.app"),
            terminator: terminator
        )

        await model.performInstall()

        XCTAssertEqual(terminator.count, 1)
    }

    func testCancelKeepsGenerationOccupiedUntilExactTaskFinishesUnwinding() async throws {
        let coordinator = UnwindingInstallCoordinator()
        let model = InstallerViewModel(
            coordinator: coordinator,
            sourceBundle: URL(fileURLWithPath: "/tmp/source.app"),
            terminator: FakeTerminator()
        )

        model.installAndStart()
        try await waitUntil { coordinator.callCount == 1 }
        model.cancel()
        try await waitUntil { coordinator.isUnwinding }
        model.installAndStart()
        XCTAssertEqual(coordinator.callCount, 1)
        XCTAssertTrue(model.isInstalling)

        coordinator.finishUnwinding()
        try await waitUntil { !model.isInstalling }
        model.installAndStart()
        try await waitUntil { coordinator.callCount == 2 }
        XCTAssertEqual(coordinator.callCount, 2)
    }
}

@MainActor
private final class UnwindingInstallCoordinator: InstallCoordinating {
    var callCount = 0
    var isUnwinding = false
    private var unwindContinuation: CheckedContinuation<Void, Never>?

    func installOrFinish(
        from sourceBundle: URL,
        onState: @escaping @MainActor (InstallerState) -> Void
    ) async throws -> InstallResult {
        callCount += 1
        if callCount == 1 {
            do {
                try await Task.sleep(for: .seconds(10))
            } catch {
                isUnwinding = true
                await withCheckedContinuation { unwindContinuation = $0 }
                throw error
            }
        }
        return .success(version: "2.0.0")
    }

    func finishUnwinding() {
        unwindContinuation?.resume()
        unwindContinuation = nil
    }
}

@MainActor
private final class FailingInstallCoordinator: InstallCoordinating {
    func installOrFinish(
        from sourceBundle: URL,
        onState: @escaping @MainActor (InstallerState) -> Void
    ) async throws -> InstallResult {
        throw InstallError.transactionFailed(primary: .health, recovery: .restoredAndRelaunched)
    }
}

@MainActor
private final class FakeTerminator: ApplicationTerminating {
    var count = 0
    func terminate() { count += 1 }
}

@MainActor
private func waitUntil(
    timeout: Duration = .seconds(1),
    condition: @escaping @MainActor () -> Bool
) async throws {
    let clock = ContinuousClock()
    let deadline = clock.now.advanced(by: timeout)
    while clock.now < deadline {
        if condition() { return }
        try await Task.sleep(for: .milliseconds(2))
    }
    XCTFail("timed out waiting for condition")
}

@MainActor
final class UninstallCommandTests: XCTestCase {
    func testDeleteDataConfirmationFailureDoesNotStopServiceOrChangeFiles() async throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        let paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))
        _ = try paths.prepareInstallLayout()
        try FileManager.default.createDirectory(at: paths.dataURL, withIntermediateDirectories: false)
        let marker = paths.dataURL.appendingPathComponent("keep")
        try Data("keep".utf8).write(to: marker)
        let service = FakeServiceController()
        service.currentState = .enabled
        let command = UninstallCommand(
            paths: paths,
            service: service,
            readConfirmation: { "wrong" },
            writeLine: { _ in }
        )

        await XCTAssertThrowsErrorAsync(try await command.execute(deleteData: true)) { error in
            XCTAssertEqual(error as? InstallError, .uninstallConfirmationRequired)
        }
        XCTAssertEqual(service.stopCount, 0)
        XCTAssertEqual(service.currentState, .enabled)
        XCTAssertEqual(try Data(contentsOf: marker), Data("keep".utf8))
    }

    func testKeychainFailureUsesUninstallSpecificStaticRecovery() async throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        let paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))
        _ = try paths.prepareInstallLayout()
        try FileManager.default.createDirectory(at: paths.dataURL, withIntermediateDirectories: false)
        let service = FakeServiceController()
        let command = UninstallCommand(
            paths: paths,
            service: service,
            readConfirmation: { UninstallCommand.confirmationToken },
            writeLine: { _ in },
            deleteCredential: { _ in throw InstallError.keychainDeletionFailed }
        )

        await XCTAssertThrowsErrorAsync(try await command.execute(deleteData: true)) { error in
            XCTAssertEqual(error as? InstallError, .keychainDeletionFailed)
            XCTAssertFalse((error as? InstallError)?.recoveryAction.contains("Login Items") ?? true)
        }
    }

    func testRootReplacementBeforeDeleteFailsClosedWithoutTouchingOutside() async throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        let paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))
        let layoutIdentity = try paths.prepareInstallLayout()
        let command = UninstallCommand(paths: paths, rootIdentity: layoutIdentity.root, service: FakeServiceController())
        let outside = temporary.appendingPathComponent("outside", isDirectory: true)
        try FileManager.default.createDirectory(at: outside, withIntermediateDirectories: false)
        let marker = outside.appendingPathComponent("keep")
        try Data("keep".utf8).write(to: marker)
        try FileManager.default.moveItem(at: paths.rootURL, to: temporary.appendingPathComponent("old-root"))
        try FileManager.default.createSymbolicLink(at: paths.rootURL, withDestinationURL: outside)

        await XCTAssertThrowsErrorAsync(try await command.execute(deleteData: false)) { error in
            XCTAssertEqual(error as? InstallError, .unsafePath)
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: marker.path))
    }

    func testAppReplacementAfterCaptureBeforeUninstallQuarantineFailsClosed() async throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        let paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))
        let layoutIdentity = try paths.prepareInstallLayout()
        let fileSystem = FaultingInstallFileSystem(failActivation: false)
        fileSystem.replaceRootChildBeforeQuarantine = true
        let command = UninstallCommand(
            paths: paths,
            rootIdentity: layoutIdentity.root,
            service: FakeServiceController(),
            fileSystem: fileSystem
        )

        await XCTAssertThrowsErrorAsync(try await command.execute(deleteData: false)) { error in
            XCTAssertEqual(error as? InstallError, .unsafePath)
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: paths.appDirectoryURL.path))
    }
}

final class BundleValidatorSigningTests: XCTestCase {
    func testCandidateAndNestedExecutablesMustShareExpectedTeamIdentifier() throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        let bundle = temporary.appendingPathComponent(".staging-candidate", isDirectory: true)
        try makeValidBundle(at: bundle)
        let validator = BundleValidator(
            expectedTeamIdentifier: "TEAM123456",
            signatureChecker: FakeSignatureChecker(teams: ["PCAPlatformBridge": "OTHER12345"]),
            architectureChecker: FakeArchitectureChecker()
        )

        XCTAssertThrowsError(try validator.validate(candidate: bundle, replacing: nil)) { error in
            XCTAssertEqual(error as? InstallError, .invalidBundle)
        }
    }

    func testEnvironmentCannotOverrideCurrentlySignedInstallerTeamAnchor() throws {
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        let bundle = temporary.appendingPathComponent(".staging-candidate", isDirectory: true)
        try makeValidBundle(at: bundle)
        setenv("PCA_EXPECTED_TEAM_ID", "ENVTEAM123", 1)
        defer { unsetenv("PCA_EXPECTED_TEAM_ID") }
        let validator = BundleValidator(
            signatureChecker: FakeSignatureChecker(
                teams: [
                    Bundle.main.bundleURL.lastPathComponent: "CURRENT1234",
                    bundle.lastPathComponent: "ENVTEAM123",
                    "pca-agentd": "ENVTEAM123",
                    "PCAPlatformBridge": "ENVTEAM123",
                ]
            ),
            architectureChecker: FakeArchitectureChecker()
        )

        XCTAssertThrowsError(try validator.validate(candidate: bundle, replacing: nil)) { error in
            XCTAssertEqual(error as? InstallError, .invalidBundle)
        }
    }

    private func makeValidBundle(at bundle: URL) throws {
        let executable = bundle.appendingPathComponent("Contents/MacOS/PersonalComputerAgent")
        let agent = bundle.appendingPathComponent("Contents/Resources/bin/pca-agentd")
        let bridge = bundle.appendingPathComponent("Contents/Resources/bin/PCAPlatformBridge")
        let launchAgent = bundle.appendingPathComponent("Contents/Library/LaunchAgents/com.pca.agentd.plist")
        try FileManager.default.createDirectory(at: executable.deletingLastPathComponent(), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: agent.deletingLastPathComponent(), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: launchAgent.deletingLastPathComponent(), withIntermediateDirectories: true)
        for binary in [executable, agent, bridge] {
            try Data("binary".utf8).write(to: binary)
            try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: binary.path)
        }
        let info: [String: Any] = [
            "CFBundleIdentifier": "com.pca.PersonalComputerAgent",
            "CFBundleExecutable": "PersonalComputerAgent",
            "CFBundleShortVersionString": "2.0.0",
            "LSUIElement": true,
        ]
        XCTAssertTrue((info as NSDictionary).write(to: bundle.appendingPathComponent("Contents/Info.plist"), atomically: true))
        let plist: [String: Any] = [
            "Label": "com.pca.agentd",
            "BundleProgram": "Contents/Resources/bin/pca-agentd",
            "ProgramArguments": ["pca-agentd", "run"],
            "RunAtLoad": true,
            "KeepAlive": true,
        ]
        XCTAssertTrue((plist as NSDictionary).write(to: launchAgent, atomically: true))
        try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: launchAgent.path)
    }
}

private struct FakeSignatureChecker: SignatureChecking {
    let teams: [String: String]
    func verifyAndReadTeamIdentifier(of target: URL) throws -> String {
        teams[target.lastPathComponent] ?? "TEAM123456"
    }
}

private struct FakeArchitectureChecker: ArchitectureChecking {
    func architectures(of executable: URL) throws -> [String] { ["arm64"] }
}

@MainActor
private func XCTAssertThrowsErrorAsync<T>(
    _ expression: @autoclosure () async throws -> T,
    _ handler: (Error) -> Void = { _ in }
) async {
    do {
        _ = try await expression()
        XCTFail("Expected error")
    } catch {
        handler(error)
    }
}
