import Foundation
import XCTest
@testable import PersonalComputerAgent

@MainActor
final class PairingCoordinatorTests: XCTestCase {
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
