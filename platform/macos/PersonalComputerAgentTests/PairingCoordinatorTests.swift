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
}
