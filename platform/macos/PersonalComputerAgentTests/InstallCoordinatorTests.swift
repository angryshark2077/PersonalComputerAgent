import Foundation
import XCTest
@testable import PersonalComputerAgent

@MainActor
final class InstallCoordinatorTests: XCTestCase {
    func testFirstInstallStagesBundlePreservesDataAndRelaunchesInstalledExecutable() async throws {
        let fixture = try Fixture(installedVersion: nil, candidateVersion: "1.0.0")
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

    func testFailedUpgradeHealthRestoresOldBundleWithoutDeletingData() async throws {
        let fixture = try Fixture(installedVersion: "1.0.0", candidateVersion: "2.0.0")
        try Data("durable".utf8).write(to: fixture.paths.dataURL.appendingPathComponent("fact"))
        _ = try await fixture.coordinator.prepareInstallation(from: fixture.candidate)
        fixture.health.isHealthy = false

        await XCTAssertThrowsErrorAsync(try await fixture.coordinator.finishInstalledSetup()) { error in
            XCTAssertEqual(error as? InstallError, .healthCheckFailedRolledBack)
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
}

@MainActor
private final class Fixture {
    let temporary: URL
    let paths: InstallPaths
    let candidate: URL
    let service = FakeServiceController()
    let health = FakeHealthChecker()
    let relauncher = FakeRelauncher()
    let coordinator: InstallCoordinator

    init(installedVersion: String?, candidateVersion: String) throws {
        temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        paths = try InstallPaths(rootURL: temporary.appendingPathComponent("root", isDirectory: true))
        try paths.createDirectories()
        candidate = temporary.appendingPathComponent("candidate.app", isDirectory: true)
        try Self.makeBundle(at: candidate, version: candidateVersion)
        if let installedVersion {
            try Self.makeBundle(at: paths.installedBundleURL, version: installedVersion)
        }
        coordinator = InstallCoordinator(
            paths: paths,
            validator: TestBundleValidator(),
            service: service,
            health: health,
            relauncher: relauncher
        )
    }

    deinit { try? FileManager.default.removeItem(at: temporary) }

    func version(at bundle: URL) throws -> String {
        try String(contentsOf: bundle.appendingPathComponent("version"), encoding: .utf8)
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
    var stopCount = 0
    var registerCount = 0
    func stopAndUnregister() async throws { stopCount += 1 }
    func registerAndWaitForApproval(onWaitingForApproval: @escaping @MainActor () -> Void) async throws {
        registerCount += 1
    }
}

@MainActor
private final class FakeHealthChecker: HealthChecking {
    var isHealthy = true
    var checkCount = 0
    func waitForHealthy(paths: InstallPaths, timeout: Duration) async throws -> Bool {
        checkCount += 1
        return isHealthy
    }
}

@MainActor
private final class FakeRelauncher: Relaunching {
    var urls: [URL] = []
    func relaunch(executable: URL, arguments: [String]) throws { urls.append(executable) }
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
