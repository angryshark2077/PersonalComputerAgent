import Foundation
import Security

protocol SignatureChecking: Sendable {
    func verifyAndReadTeamIdentifier(of target: URL) throws -> String
}

protocol ArchitectureChecking: Sendable {
    func architectures(of executable: URL) throws -> [String]
}

struct ProductionSignatureChecker: SignatureChecking {
    func verifyAndReadTeamIdentifier(of target: URL) throws -> String {
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(target as CFURL, [], &code) == errSecSuccess,
              let code
        else { throw InstallError.invalidBundle }
        let validationFlags = SecCSFlags(rawValue: kSecCSStrictValidate | kSecCSCheckAllArchitectures)
        guard SecStaticCodeCheckValidity(code, validationFlags, nil) == errSecSuccess else {
            throw InstallError.invalidBundle
        }
        var signingInformation: CFDictionary?
        guard SecCodeCopySigningInformation(
            code,
            SecCSFlags(rawValue: kSecCSSigningInformation),
            &signingInformation
        ) == errSecSuccess,
            let information = signingInformation as? [String: Any],
            let team = information[kSecCodeInfoTeamIdentifier as String] as? String,
            !team.isEmpty
        else { throw InstallError.invalidBundle }
        return team
    }
}

struct ProductionArchitectureChecker: ArchitectureChecking {
    func architectures(of executable: URL) throws -> [String] {
        let handle = try FileHandle(forReadingFrom: executable)
        defer { try? handle.close() }
        guard let header = try handle.read(upToCount: 8), header.count == 8 else {
            throw InstallError.invalidBundle
        }
        let bytes = [UInt8](header)
        let magic = littleEndianUInt32(bytes, offset: 0)
        guard magic == 0xFEED_FACF else { throw InstallError.invalidBundle }
        let cpuType = littleEndianUInt32(bytes, offset: 4)
        switch cpuType {
        case 0x0100_000C: return ["arm64"]
        case 0x0100_0007: return ["x86_64"]
        default: throw InstallError.invalidBundle
        }
    }

    private func littleEndianUInt32(_ bytes: [UInt8], offset: Int) -> UInt32 {
        UInt32(bytes[offset])
            | UInt32(bytes[offset + 1]) << 8
            | UInt32(bytes[offset + 2]) << 16
            | UInt32(bytes[offset + 3]) << 24
    }
}

struct BundleValidator: BundleValidating {
    static let bundleIdentifier = "com.pca.PersonalComputerAgent"
    static let launchAgentName = "com.pca.agentd.plist"
    static let productionTeamIdentifier = "UHB669QQ6A"

    private let signatureChecker: any SignatureChecking
    private let architectureChecker: any ArchitectureChecking
    private let fileManager: FileManager
    private let expectedTeamIdentifier: String?

    init(
        expectedTeamIdentifier: String? = nil,
        signatureChecker: any SignatureChecking = ProductionSignatureChecker(),
        architectureChecker: any ArchitectureChecking = ProductionArchitectureChecker(),
        fileManager: FileManager = .default
    ) {
        self.signatureChecker = signatureChecker
        self.architectureChecker = architectureChecker
        self.fileManager = fileManager
        self.expectedTeamIdentifier = expectedTeamIdentifier ?? Self.productionTeamIdentifier
    }

    func validate(candidate: URL, replacing installed: URL?) throws -> ValidatedBundle {
        guard candidate.pathExtension == "app" || candidate.lastPathComponent.hasPrefix(".staging-") else {
            throw InstallError.invalidBundle
        }
        let candidateValues = try candidate.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey])
        guard candidateValues.isDirectory == true, candidateValues.isSymbolicLink != true else {
            throw InstallError.invalidBundle
        }
        try rejectSymlinksAndRuntimeDirectories(in: candidate)
        let info = try dictionary(at: candidate.appendingPathComponent("Contents/Info.plist"))
        guard info["CFBundleIdentifier"] as? String == Self.bundleIdentifier,
              info["CFBundleExecutable"] as? String == "PersonalComputerAgent",
              info["LSUIElement"] as? Bool == true,
              !(info["NSScreenCaptureUsageDescription"] as? String ?? "").isEmpty,
              !(info["NSPhotoLibraryUsageDescription"] as? String ?? "").isEmpty,
              let candidateVersion = info["CFBundleShortVersionString"] as? String,
              Version(candidateVersion).isValid
        else { throw InstallError.invalidBundle }

        let executable = candidate.appendingPathComponent("Contents/MacOS/PersonalComputerAgent")
        let agent = candidate.appendingPathComponent("Contents/Resources/bin/pca-agentd")
        let bridge = candidate.appendingPathComponent(
            "Contents/Helpers/PCAPlatformBridge.app/Contents/MacOS/PCAPlatformBridge"
        )
        let bridgeInfo = try dictionary(
            at: candidate.appendingPathComponent("Contents/Helpers/PCAPlatformBridge.app/Contents/Info.plist")
        )
        guard bridgeInfo["CFBundleIdentifier"] as? String == "com.pca.PersonalComputerAgent.PlatformBridge",
              bridgeInfo["CFBundleExecutable"] as? String == "PCAPlatformBridge",
              bridgeInfo["LSUIElement"] as? Bool == true,
              !(bridgeInfo["NSLocationWhenInUseUsageDescription"] as? String ?? "").isEmpty,
              !(bridgeInfo["NSScreenCaptureUsageDescription"] as? String ?? "").isEmpty,
              !(bridgeInfo["NSPhotoLibraryUsageDescription"] as? String ?? "").isEmpty
        else { throw InstallError.invalidBundle }
        let wechatRepair = candidate.appendingPathComponent("Contents/Resources/bin/pca-wechat-repair")
        let ffmpeg = candidate.appendingPathComponent("Contents/Resources/bin/ffmpeg")
        let launchAgent = candidate.appendingPathComponent("Contents/Library/LaunchAgents/\(Self.launchAgentName)")
        for binary in [executable, agent, bridge, wechatRepair, ffmpeg] {
            try requireRegularFile(binary, executable: true)
            guard try architectureChecker.architectures(of: binary) == ["arm64"] else {
                throw InstallError.invalidBundle
            }
        }
        try requireRegularFile(launchAgent, executable: false)
        try validateLaunchAgent(launchAgent)

        let previousVersion = try installed.map(version(at:))
        if let previousVersion,
           Version(candidateVersion).compare(to: Version(previousVersion)) == .orderedAscending {
            throw InstallError.downgradeRejected(installed: previousVersion, candidate: candidateVersion)
        }
        guard let expectedTeamIdentifier, !expectedTeamIdentifier.isEmpty else {
            throw InstallError.invalidBundle
        }
        let candidateTeamIdentifier = try signatureChecker.verifyAndReadTeamIdentifier(of: candidate)
        guard candidateTeamIdentifier == expectedTeamIdentifier else { throw InstallError.invalidBundle }
        for signedTarget in [candidate, agent, bridge, wechatRepair, ffmpeg] {
            guard try signatureChecker.verifyAndReadTeamIdentifier(of: signedTarget) == expectedTeamIdentifier else {
                throw InstallError.invalidBundle
            }
        }
        let previousTeamIdentifier = try installed.map {
            try signatureChecker.verifyAndReadTeamIdentifier(of: $0)
        }
        return ValidatedBundle(
            version: candidateVersion,
            previousVersion: previousVersion,
            signingIdentityChanged: previousTeamIdentifier.map { $0 != candidateTeamIdentifier } ?? false
        )
    }

    func version(at bundle: URL) throws -> String {
        let info = try dictionary(at: bundle.appendingPathComponent("Contents/Info.plist"))
        guard info["CFBundleIdentifier"] as? String == Self.bundleIdentifier,
              let version = info["CFBundleShortVersionString"] as? String,
              Version(version).isValid
        else { throw InstallError.invalidBundle }
        return version
    }

    private func validateLaunchAgent(_ url: URL) throws {
        let plist = try dictionary(at: url)
        guard plist["Label"] as? String == "com.pca.agentd",
              plist["BundleProgram"] as? String == "Contents/Resources/bin/pca-agentd",
              plist["ProgramArguments"] as? [String] == ["pca-agentd", "run"],
              plist["RunAtLoad"] as? Bool == true,
              plist["KeepAlive"] as? Bool == true
        else { throw InstallError.invalidBundle }
    }

    private func dictionary(at url: URL) throws -> [String: Any] {
        guard let dictionary = NSDictionary(contentsOf: url) as? [String: Any] else {
            throw InstallError.invalidBundle
        }
        return dictionary
    }

    private func requireRegularFile(_ url: URL, executable: Bool) throws {
        let attributes = try fileManager.attributesOfItem(atPath: url.path)
        guard attributes[.type] as? FileAttributeType == .typeRegular,
              let permissions = attributes[.posixPermissions] as? NSNumber
        else { throw InstallError.invalidBundle }
        let mode = permissions.intValue
        guard mode & 0o022 == 0 else { throw InstallError.invalidBundle }
        if executable {
            guard mode & 0o111 == 0o111 else { throw InstallError.invalidBundle }
        } else {
            guard mode & 0o111 == 0 else { throw InstallError.invalidBundle }
        }
    }

    private func rejectSymlinksAndRuntimeDirectories(in bundle: URL) throws {
        guard let enumerator = fileManager.enumerator(
            at: bundle,
            includingPropertiesForKeys: [.isSymbolicLinkKey, .isDirectoryKey],
            options: [],
            errorHandler: { _, _ in false }
        ) else { throw InstallError.invalidBundle }
        for case let url as URL in enumerator {
            let values = try url.resourceValues(forKeys: [.isSymbolicLinkKey, .isDirectoryKey])
            if values.isSymbolicLink == true { throw InstallError.invalidBundle }
            if values.isDirectory == true, ["Data", "Run"].contains(url.lastPathComponent) {
                throw InstallError.invalidBundle
            }
        }
    }
}
