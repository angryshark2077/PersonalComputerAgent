import Foundation
import Security

@MainActor
struct UninstallCommand {
    static let confirmationToken = "DELETE PCA DATA"
    static let credentialScopes = [
        KeychainScope(service: "com.pca.bridge", account: "shared-secret-v1")
    ]

    let paths: InstallPaths
    let rootIdentity: FileIdentity?
    let service: any ServiceControlling
    let fileSystem: any InstallFileOperating
    var readConfirmation: () -> String?
    var writeLine: (String) -> Void
    var deleteCredential: @MainActor (KeychainScope) throws -> Void

    init(
        paths: InstallPaths,
        rootIdentity: FileIdentity? = nil,
        service: any ServiceControlling,
        fileSystem: any InstallFileOperating = LocalInstallFileSystem(),
        readConfirmation: @escaping () -> String? = { readLine() },
        writeLine: @escaping (String) -> Void = { print($0) },
        deleteCredential: @escaping @MainActor (KeychainScope) throws -> Void = Self.deleteCredential
    ) {
        self.paths = paths
        self.rootIdentity = rootIdentity
        self.service = service
        self.fileSystem = fileSystem
        self.readConfirmation = readConfirmation
        self.writeLine = writeLine
        self.deleteCredential = deleteCredential
    }

    func execute(deleteData: Bool) async throws {
        try await service.stopAndUnregister()
        guard InstallPaths.entryExists(paths.rootURL) else { return }
        let capturedIdentity = try rootIdentity ?? InstallPaths.identity(of: paths.rootURL)
        try paths.revalidateRoot(capturedIdentity)

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

        try removeIfPresent(paths.appDirectoryURL, directChildOf: paths.rootURL, rootIdentity: capturedIdentity)
        try removeIfPresent(paths.runURL, directChildOf: paths.rootURL, rootIdentity: capturedIdentity)
        if deleteData {
            try removeIfPresent(paths.dataURL, directChildOf: paths.rootURL, rootIdentity: capturedIdentity)
            for scope in Self.credentialScopes {
                try deleteCredential(scope)
            }
        }
    }

    private func removeIfPresent(_ target: URL, directChildOf parent: URL, rootIdentity: FileIdentity) throws {
        guard fileSystem.exists(target) else { return }
        try fileSystem.quarantineAndDelete(
            target,
            parent: parent,
            paths: paths,
            rootIdentity: rootIdentity
        )
    }

    private static func deleteCredential(_ scope: KeychainScope) throws {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: scope.service,
            kSecAttrAccount: scope.account
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw InstallError.keychainDeletionFailed
        }
    }
}

struct KeychainScope: Equatable, Sendable {
    let service: String
    let account: String
}
