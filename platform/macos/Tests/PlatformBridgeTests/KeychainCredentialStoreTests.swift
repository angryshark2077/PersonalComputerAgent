import Security
@testable import BridgeProtocol
import XCTest

final class KeychainCredentialStoreTests: XCTestCase {
    func testBridgeCredentialExistenceLookupNeverRequestsSecretData() {
        let query = KeychainCredentialStore.bridgeCredentialExistenceQuery()

        XCTAssertEqual(query[kSecClass as String] as? String, kSecClassGenericPassword as String)
        XCTAssertEqual(query[kSecAttrService as String] as? String, "com.pca.bridge")
        XCTAssertEqual(query[kSecAttrAccount as String] as? String, "shared-secret-v1")
        XCTAssertNil(query[kSecReturnData as String])
    }

    func testWechatPlaceholderLookupNeverRequestsSecretData() {
        let query = KeychainCredentialStore.wechatCredentialExistenceQuery()

        XCTAssertEqual(query[kSecClass as String] as? String, kSecClassGenericPassword as String)
        XCTAssertEqual(query[kSecAttrService as String] as? String, "com.pca.wechat")
        XCTAssertEqual(query[kSecAttrAccount as String] as? String, "current-v1")
        XCTAssertNil(query[kSecReturnData as String])
        XCTAssertNil(query[kSecValueData as String])
    }
}
