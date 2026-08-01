import Foundation
import XCTest
@testable import PersonalComputerAgent

@MainActor
final class PairingCoordinatorTests: XCTestCase {
    func testInstalledPairingConfigurationUsesFixedSocketAndCloudOrigin() throws {
        let root = URL(fileURLWithPath: "/Users/test/Library/Application Support/PersonalComputerAgent")
        let configuration = try PairingIPCConfiguration.production(rootURL: root)

        XCTAssertEqual(configuration.socketURL.path, root.appendingPathComponent("Run/pairing.sock").path)
        XCTAssertEqual(
            configuration.cloudAPIOrigin.absoluteString,
            "https://pca-cloud-api-production.up.railway.app"
        )
    }

    func testPairingIPCProofMatchesAgentTranscript() {
        XCTAssertEqual(
            PairingIPCAuthentication.proof(
                secret: Data(repeating: 0x42, count: 32),
                nonce: Data(repeating: 0x24, count: 32),
                context: "pca-setup-pairing-v1:01982222-7222-8222-8222-222222222222:status"
            ),
            "kvpCQr/kdLp9/RoEDg8Z5mPHck5zJ7KmKi9BGiOf/p0="
        )
    }

    func testPairingIPCRequestUsesAgentEnvelopeAndURLSafeNonce() throws {
        let request = try PairingIPCRequest<TestBeginPayload>.make(
            operation: .begin,
            payload: TestBeginPayload(
                callbackURI: "http://127.0.0.1:49152/pca/pair/callback",
                cloudAPIOrigin: "https://pca-cloud-api-production.up.railway.app"
            ),
            secret: Data(repeating: 0x42, count: 32),
            nonce: Data(repeating: 0x24, count: 32),
            requestID: UUID(uuidString: "01982222-7222-8222-8222-222222222222")!
        )
        let envelope = try XCTUnwrap(try JSONSerialization.jsonObject(with: request) as? [String: Any])

        XCTAssertEqual(envelope["protocol_version"] as? Int, 1)
        XCTAssertEqual(envelope["request_id"] as? String, "01982222-7222-8222-8222-222222222222")
        XCTAssertEqual(envelope["operation"] as? String, "begin")
        XCTAssertEqual(envelope["nonce"] as? String, "JCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQ")
        XCTAssertEqual(envelope["proof"] as? String, "ehqKwu3rkOVCMnCS/1DV2Tutdi63kFKVxuhqDqUaVS0=")
        XCTAssertEqual(
            (envelope["payload"] as? [String: String])?["cloud_api_origin"],
            "https://pca-cloud-api-production.up.railway.app"
        )
    }

    func testCallbackRejectsWrongStateAndStopsListener() async throws {
        let coordinator = PairingCoordinator.fake(state: "expected")

        do {
            try await coordinator.accept(URL(string: "http://127.0.0.1/pca/pair/callback?code=x&state=wrong")!)
            XCTFail("wrong callback state must fail")
        } catch let error as PairingError {
            XCTAssertEqual(error, .stateMismatch)
        }

        XCTAssertTrue(coordinator.listenerIsClosed)
    }

    func testCallbackRejectsDuplicateCodeOrStateAndStopsListener() async throws {
        for query in ["code=first&code=second&state=expected", "code=x&state=expected&state=again"] {
            let coordinator = PairingCoordinator.fake(state: "expected")
            let callback = URL(string: "http://127.0.0.1/pca/pair/callback?\(query)")!

            do {
                _ = try await coordinator.accept(callback)
                XCTFail("duplicate callback query must fail")
            } catch let error as PairingError {
                XCTAssertEqual(error, .invalidCallback)
            }

            XCTAssertTrue(coordinator.listenerIsClosed)
        }
    }

    func testPairingExpiryClosesListenerAndCancelsAgentSession() async throws {
        let agent = ExpiringPairingAgent()
        let coordinator = PairingCoordinator(
            agent: agent,
            browser: SuccessfulPairingBrowser(),
            callbackTimeout: .milliseconds(10)
        )

        do {
            _ = try await coordinator.repair()
            XCTFail("an unanswered pairing request must expire")
        } catch let error as PairingError {
            XCTAssertEqual(error, .expired)
        }

        XCTAssertTrue(coordinator.listenerIsClosed)
        XCTAssertEqual(agent.cancelledSessionID, agent.sessionID)
    }

    func testCancelPairingStopsTheActiveSessionImmediately() async throws {
        let agent = ExpiringPairingAgent()
        let pairing = PairingCoordinator(
            agent: agent,
            browser: SuccessfulPairingBrowser(),
            callbackTimeout: .seconds(10)
        )
        let model = InstallerViewModel(
            coordinator: ImmediateInstallCoordinator(),
            sourceBundle: URL(fileURLWithPath: "/tmp/source.app"),
            pairingCoordinator: pairing,
            terminator: TestTerminator()
        )

        model.repairPairing()
        try await waitForPairing { agent.didBegin && model.isPairing }
        model.cancelPairing()

        try await waitForPairing { !model.isPairing }
        XCTAssertTrue(pairing.listenerIsClosed)
        XCTAssertEqual(agent.cancelledSessionID, agent.sessionID)
        guard case .repair = model.state else {
            return XCTFail("cancelled pairing must return to repair")
        }
    }
}

private struct TestBeginPayload: Encodable, Sendable {
    let callbackURI: String
    let cloudAPIOrigin: String

    enum CodingKeys: String, CodingKey {
        case callbackURI = "callback_uri"
        case cloudAPIOrigin = "cloud_api_origin"
    }
}

@MainActor
private final class ExpiringPairingAgent: PairingAgentHandingOff {
    let sessionID = "01982222-7222-8222-8222-222222222222"
    private(set) var cancelledSessionID: String?
    private(set) var didBegin = false

    func isPaired() async throws -> Bool { false }

    func beginPairing(_: PairingStartHandoff) async throws -> PairingSessionHandoff {
        didBegin = true
        return PairingSessionHandoff(
            sessionID: sessionID,
            authorizationURL: URL(string: "https://pca-dashboard-production.up.railway.app/pair")!,
            callbackState: String(repeating: "s", count: 43)
        )
    }

    func completePairing(_: PairingCallbackHandoff) async throws -> PairingResult {
        XCTFail("expired pairing must not complete")
        return .alreadyPaired
    }

    func cancelPairing(sessionID: String) async {
        cancelledSessionID = sessionID
    }
}

@MainActor
private final class SuccessfulPairingBrowser: PairingBrowserOpening {
    func open(_: URL) -> Bool { true }
}

@MainActor
private final class ImmediateInstallCoordinator: InstallCoordinating {
    func installOrFinish(
        from _: URL,
        onState: @escaping @MainActor (InstallerState) -> Void
    ) async throws -> InstallResult {
        .success(version: "0.1.10")
    }
}

@MainActor
private final class TestTerminator: ApplicationTerminating {
    func terminate() {}
}

@MainActor
private func waitForPairing(
    timeout: Duration = .seconds(1),
    condition: @escaping @MainActor () -> Bool
) async throws {
    let clock = ContinuousClock()
    let deadline = clock.now.advanced(by: timeout)
    while clock.now < deadline {
        if condition() { return }
        try await Task.sleep(for: .milliseconds(2))
    }
    XCTFail("timed out waiting for pairing state")
}
