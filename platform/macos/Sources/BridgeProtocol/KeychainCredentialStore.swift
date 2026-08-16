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
    public static let deviceService = "com.pca.device"
    public static let deviceAccount = "current-v1"
    public static let wechatService = "com.pca.wechat"
    public static let wechatAccount = "current-v1"

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

    /// Checks whether an existing Bridge credential can be preserved during an update without
    /// asking the installer to read or replace its secret.
    public func bridgeCredentialExists() throws -> Bool {
        let status = SecItemCopyMatching(Self.bridgeCredentialExistenceQuery() as CFDictionary, nil)
        if status == errSecSuccess { return true }
        if status == errSecItemNotFound { return false }
        throw Self.error(for: status)
    }

    public func store(_ secret: Data, trustedApplicationURLs: [URL]) throws {
        guard secret.count == Self.sharedSecretLength else {
            throw KeychainCredentialStoreError.invalidSecretLength
        }
        let access = try Self.makeAccess(trustedApplicationURLs: trustedApplicationURLs)

        let query = Self.baseQuery()
        var item = query
        item[kSecValueData as String] = secret
        item[kSecAttrAccess as String] = access
        let addStatus = SecItemAdd(item as CFDictionary, nil)
        if addStatus == errSecSuccess { return }
        if addStatus == errSecDuplicateItem {
            let updateStatus = SecItemUpdate(
                query as CFDictionary,
                [
                    kSecValueData as String: secret,
                    kSecAttrAccess as String: access,
                ] as CFDictionary
            )
            guard updateStatus == errSecSuccess else {
                throw Self.error(for: updateStatus)
            }
            return
        }
        throw Self.error(for: addStatus)
    }

    public func delete() throws {
        let status = SecItemDelete(Self.baseQuery() as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw Self.error(for: status)
        }
    }

    public func updateAccessIfPresent(
        service: String,
        account: String,
        label: String,
        trustedApplicationURLs: [URL]
    ) throws {
        let query = Self.baseQuery(service: service, account: account)
        let lookupStatus = SecItemCopyMatching(query as CFDictionary, nil)
        if lookupStatus == errSecItemNotFound { return }
        guard lookupStatus == errSecSuccess else { throw Self.error(for: lookupStatus) }
        let access = try Self.makeAccess(
            trustedApplicationURLs: trustedApplicationURLs,
            label: label
        )
        let status = SecItemUpdate(
            query as CFDictionary,
            [kSecAttrAccess as String: access] as CFDictionary
        )
        guard status == errSecSuccess else { throw Self.error(for: status) }
    }

    /// Creates the device item once with the installed-app ACL.
    ///
    /// `agentd` only updates this item after its token exchange, preserving the ACL rather than
    /// creating an unrestricted credential if pairing is interrupted before the first exchange.
    public func ensureDeviceCredentialPlaceholder(trustedApplicationURLs: [URL]) throws {
        let query = Self.baseQuery(service: Self.deviceService, account: Self.deviceAccount)
        var result: CFTypeRef?
        let lookupStatus = SecItemCopyMatching(query as CFDictionary, &result)
        if lookupStatus == errSecSuccess { return }
        guard lookupStatus == errSecItemNotFound else { throw Self.error(for: lookupStatus) }

        let access = try Self.makeAccess(
            trustedApplicationURLs: trustedApplicationURLs,
            label: "Personal Computer Agent Device Credential"
        )
        var item = query
        // This deliberately invalid record is never accepted as a paired credential. It reserves
        // the item with the correct ACL so `agentd` can replace its value after the token exchange.
        item[kSecValueData as String] = Data([0])
        item[kSecAttrAccess as String] = access
        let addStatus = SecItemAdd(item as CFDictionary, nil)
        guard addStatus == errSecSuccess else { throw Self.error(for: addStatus) }
    }

    /// Creates the `WeChat` credential item with access limited to the installed
    /// repair tool and `agentd`. An invalid placeholder keeps collection fail-closed until repair.
    public func ensureWechatCredentialPlaceholder(trustedApplicationURLs: [URL]) throws {
        let query = Self.wechatCredentialExistenceQuery()
        // Query only for existence. Requesting kSecReturnData here asks the installer to read a
        // secret whose ACL intentionally trusts only agentd and the repair tool, which causes an
        // unnecessary login-keychain password prompt on every upgrade.
        let lookupStatus = SecItemCopyMatching(query as CFDictionary, nil)
        if lookupStatus == errSecSuccess { return }
        guard lookupStatus == errSecItemNotFound else { throw Self.error(for: lookupStatus) }

        let access = try Self.makeAccess(
            trustedApplicationURLs: trustedApplicationURLs,
            label: "Personal Computer Agent WeChat Credential"
        )
        var item = query
        item[kSecValueData as String] = Data([0])
        item[kSecAttrAccess as String] = access
        let addStatus = SecItemAdd(item as CFDictionary, nil)
        guard addStatus == errSecSuccess else { throw Self.error(for: addStatus) }
    }

    static func wechatCredentialExistenceQuery() -> [String: Any] {
        Self.baseQuery(service: Self.wechatService, account: Self.wechatAccount)
    }

    static func bridgeCredentialExistenceQuery() -> [String: Any] {
        Self.baseQuery()
    }

    private static func baseQuery(
        service: String = Self.service,
        account: String = Self.account
    ) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    private static func makeAccess(trustedApplicationURLs: [URL], label: String = "Personal Computer Agent Bridge Credential") throws -> SecAccess {
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
            label as CFString,
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
