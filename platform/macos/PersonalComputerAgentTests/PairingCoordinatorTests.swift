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
}

private struct TestBeginPayload: Encodable, Sendable {
    let callbackURI: String
    let cloudAPIOrigin: String

    enum CodingKeys: String, CodingKey {
        case callbackURI = "callback_uri"
        case cloudAPIOrigin = "cloud_api_origin"
    }
}
