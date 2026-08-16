import Foundation
import XCTest
@testable import PersonalComputerAgent

@MainActor
final class PairingCoordinatorTests: XCTestCase {
    func testCallbackListenerIgnoresInvalidLocalRequestBeforeValidCallback() async throws {
        let state = String(repeating: "s", count: 43)
        let server = try PairingCallbackServer(expectedState: state)
        let callbackURL = try await server.start()
        let waitTask = Task { try await server.waitForCallback() }

        var invalid = URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)!
        invalid.path = "/favicon.ico"
        let (_, invalidResponse) = try await URLSession.shared.data(from: invalid.url!)
        XCTAssertEqual((invalidResponse as? HTTPURLResponse)?.statusCode, 400)
        XCTAssertFalse(server.isClosed)

        var valid = URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)!
        valid.queryItems = [
            URLQueryItem(name: "code", value: "authorization-code"),
            URLQueryItem(name: "state", value: state),
        ]
        let (_, validResponse) = try await URLSession.shared.data(from: valid.url!)
        XCTAssertEqual((validResponse as? HTTPURLResponse)?.statusCode, 200)
        let receivedURL = try await waitTask.value
        XCTAssertEqual(
            PairingCallback.parse(receivedURL),
            PairingCallback(authorizationCode: "authorization-code", state: state)
        )
        XCTAssertTrue(server.isClosed)
    }

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

    func testAmbiguousCompletionReturnsAlreadyPairedWhenAgentPersistedTheCredential() async throws {
        let agent = CallbackPairingAgent(completionResults: [.failure(.agentRejected)])
        agent.pairedAfterFailedCompletion = true
        let coordinator = PairingCoordinator(agent: agent, browser: SuccessfulPairingBrowser())

        let result = try await coordinator.repair()

        XCTAssertEqual(result, .alreadyPaired)
        XCTAssertEqual(agent.beginCount, 1)
    }

    func testRetryStartsAFreshCallbackAfterAFailedCompletion() async throws {
        let agent = CallbackPairingAgent(completionResults: [
            .failure(.agentRejected),
            .success(.paired(deviceID: "device", workspaceID: "workspace")),
        ])
        let coordinator = PairingCoordinator(agent: agent, browser: SuccessfulPairingBrowser())

        do {
            _ = try await coordinator.repair()
            XCTFail("the first rejected completion must fail")
        } catch let error as PairingError {
            XCTAssertEqual(error, .agentRejected)
        }
        let result = try await coordinator.repair()

        XCTAssertEqual(result, .paired(deviceID: "device", workspaceID: "workspace"))
        XCTAssertEqual(agent.beginCount, 2)
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
private final class CallbackPairingAgent: PairingAgentHandingOff {
    var pairedAfterFailedCompletion = false
    private(set) var beginCount = 0
    private var completionResults: [Result<PairingResult, PairingError>]

    init(completionResults: [Result<PairingResult, PairingError>]) {
        self.completionResults = completionResults
    }

    func isPaired() async throws -> Bool { pairedAfterFailedCompletion }

    func beginPairing(_ handoff: PairingStartHandoff) async throws -> PairingSessionHandoff {
        beginCount += 1
        let state = String(repeating: Character(String(beginCount)), count: 43)
        var callback = URLComponents(url: handoff.callbackURI, resolvingAgainstBaseURL: false)!
        callback.queryItems = [
            URLQueryItem(name: "code", value: "code-\(beginCount)"),
            URLQueryItem(name: "state", value: state),
        ]
        let callbackURL = callback.url!
        Task.detached {
            try? await Task.sleep(for: .milliseconds(10))
            _ = try? await URLSession.shared.data(from: callbackURL)
        }
        return PairingSessionHandoff(
            sessionID: "01982222-7222-8222-8222-22222222222\(beginCount)",
            authorizationURL: URL(string: "https://pca-dashboard-production.up.railway.app/pair")!,
            callbackState: state
        )
    }

    func completePairing(_: PairingCallbackHandoff) async throws -> PairingResult {
        let result = completionResults.removeFirst()
        switch result {
        case let .success(value): return value
        case let .failure(error): throw error
        }
    }

    func cancelPairing(sessionID _: String) async {}
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
