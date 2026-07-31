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

    public func store(_ secret: Data, trustedApplicationURLs: [URL]) throws {
        guard secret.count == Self.sharedSecretLength else {
            throw KeychainCredentialStoreError.invalidSecretLength
        }
        let access = try Self.makeAccess(trustedApplicationURLs: trustedApplicationURLs)

        let query = Self.baseQuery()
        let replacementDeleteStatus = SecItemDelete(query as CFDictionary)
        guard replacementDeleteStatus == errSecSuccess || replacementDeleteStatus == errSecItemNotFound else {
            throw Self.error(for: replacementDeleteStatus)
        }

        var item = query
        item[kSecValueData as String] = secret
        item[kSecAttrAccess as String] = access
        let addStatus = SecItemAdd(item as CFDictionary, nil)

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

    private static func makeAccess(trustedApplicationURLs: [URL]) throws -> SecAccess {
        let normalizedURLs = trustedApplicationURLs.map(\.standardizedFileURL)
        guard !normalizedURLs.isEmpty,
              normalizedURLs.allSatisfy({ $0.isFileURL && $0.path.hasPrefix("/") }),
              Set(normalizedURLs.map(\.path)).count == normalizedURLs.count
        else {
            throw KeychainCredentialStoreError.operationFailed
        }

        var trustedApplications: [SecTrustedApplication] = []
        trustedApplications.reserveCapacity(normalizedURLs.count)
        for url in normalizedURLs {
            var trustedApplication: SecTrustedApplication?
            let status = url.path.withCString {
                SecTrustedApplicationCreateFromPath($0, &trustedApplication)
            }
            guard status == errSecSuccess, let trustedApplication else {
                throw Self.error(for: status)
            }
            trustedApplications.append(trustedApplication)
        }

        var access: SecAccess?
        let status = SecAccessCreate(
            "Personal Computer Agent Bridge Credential" as CFString,
            trustedApplications as CFArray,
            &access
        )
        guard status == errSecSuccess, let access else {
            throw Self.error(for: status)
        }
        return access
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
