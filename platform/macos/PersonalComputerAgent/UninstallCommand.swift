import Foundation
import Security

@MainActor
struct UninstallCommand {
    static let confirmationToken = "DELETE PCA DATA"
    static let credentialScopes = [
        KeychainScope(service: "com.pca.bridge", account: "shared-secret-v1")
    ]

    let paths: InstallPaths
    let service: any ServiceControlling
    var fileManager: FileManager = .default
    var readConfirmation: () -> String? = { readLine() }
    var writeLine: (String) -> Void = { print($0) }
    var deleteCredential: @MainActor (KeychainScope) throws -> Void = Self.deleteCredential

    func execute(deleteData: Bool) async throws {
        try await service.stopAndUnregister()

        if deleteData {
            writeLine("Persistent data to delete: \(paths.dataURL.path)")
            for scope in Self.credentialScopes {
                writeLine("Keychain item to delete: service=\(scope.service), account=\(scope.account)")
            }
            writeLine("Type exactly: \(Self.confirmationToken)")
            guard readConfirmation() == Self.confirmationToken else {
                throw InstallError.uninstallConfirmationRequired
            }
        }

        try removeIfPresent(paths.appDirectoryURL, directChildOf: paths.rootURL)
        try removeIfPresent(paths.runURL, directChildOf: paths.rootURL)
        if deleteData {
            try removeIfPresent(paths.dataURL, directChildOf: paths.rootURL)
            for scope in Self.credentialScopes {
                try deleteCredential(scope)
            }
        }
    }

    private func removeIfPresent(_ target: URL, directChildOf parent: URL) throws {
        guard fileManager.fileExists(atPath: target.path) else { return }
        try paths.verifyDeletionTarget(target, directChildOf: parent)
        try fileManager.removeItem(at: target)
    }

    private static func deleteCredential(_ scope: KeychainScope) throws {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: scope.service,
            kSecAttrAccount: scope.account
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw InstallError.serviceRegistrationFailed
        }
    }
}

struct KeychainScope: Equatable, Sendable {
    let service: String
    let account: String
}
