import Foundation
import Security

public enum KeychainCredentialStoreError: Error, Equatable, Sendable {
    case unavailable
    case invalidSecretLength
    case corruptSecret
    case operationFailed
}

public struct KeychainCredentialStore: Sendable {
    public static let service = "com.pca.bridge"
    public static let account = "shared-secret-v1"
    public static let sharedSecretLength = 32

    public init() {}

    public func load() throws -> Data? {
        var query = Self.baseQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)

        if status == errSecItemNotFound {
            return nil
        }
        guard status == errSecSuccess else {
            throw Self.error(for: status)
        }
        guard let secret = result as? Data else {
            throw KeychainCredentialStoreError.operationFailed
        }
        guard secret.count == Self.sharedSecretLength else {
            throw KeychainCredentialStoreError.corruptSecret
        }
        return secret
    }

    public func store(_ secret: Data) throws {
        guard secret.count == Self.sharedSecretLength else {
            throw KeychainCredentialStoreError.invalidSecretLength
        }

        let query = Self.baseQuery()
        let attributes = [kSecValueData as String: secret]
        let updateStatus = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)

        if updateStatus == errSecSuccess {
            return
        }
        guard updateStatus == errSecItemNotFound else {
            throw Self.error(for: updateStatus)
        }

        var item = query
        item[kSecValueData as String] = secret
        let addStatus = SecItemAdd(item as CFDictionary, nil)

        if addStatus == errSecDuplicateItem {
            let retryStatus = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
            guard retryStatus == errSecSuccess else {
                throw Self.error(for: retryStatus)
            }
            return
        }
        guard addStatus == errSecSuccess else {
            throw Self.error(for: addStatus)
        }
    }

    public func delete() throws {
        let status = SecItemDelete(Self.baseQuery() as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw Self.error(for: status)
        }
    }

    private static func baseQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    private static func error(for status: OSStatus) -> KeychainCredentialStoreError {
        switch status {
        case errSecNotAvailable, errSecInteractionNotAllowed:
            .unavailable
        default:
            .operationFailed
        }
    }
}
