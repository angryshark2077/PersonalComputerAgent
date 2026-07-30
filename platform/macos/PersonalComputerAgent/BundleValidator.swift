import Foundation

protocol SignatureChecking: Sendable {
    func verifyAndReadTeamIdentifier(of target: URL) throws -> String
}

protocol ArchitectureChecking: Sendable {
    func architectures(of executable: URL) throws -> [String]
}

struct ProductionSignatureChecker: SignatureChecking {
    func verifyAndReadTeamIdentifier(of target: URL) throws -> String {
        let verify = Process()
        verify.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
        verify.arguments = ["--verify", "--strict", "--verbose=2", target.path]
        verify.standardOutput = FileHandle.nullDevice
        verify.standardError = FileHandle.nullDevice
        try verify.run()
        verify.waitUntilExit()
        guard verify.terminationStatus == 0 else { throw InstallError.invalidBundle }

        let details = Pipe()
        let inspect = Process()
        inspect.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
        inspect.arguments = ["-d", "--verbose=4", target.path]
        inspect.standardOutput = FileHandle.nullDevice
        inspect.standardError = details
        try inspect.run()
        inspect.waitUntilExit()
        guard inspect.terminationStatus == 0 else { throw InstallError.invalidBundle }
        let output = String(decoding: details.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        guard let line = output.split(whereSeparator: \ .isNewline)
            .first(where: { $0.hasPrefix("TeamIdentifier=") })
        else { throw InstallError.invalidBundle }
        let team = line.dropFirst("TeamIdentifier=".count)
        guard !team.isEmpty, team != "not set" else { throw InstallError.invalidBundle }
        return String(team)
    }
}

struct ProductionArchitectureChecker: ArchitectureChecking {
    func architectures(of executable: URL) throws -> [String] {
        let output = Pipe()
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/lipo")
        process.arguments = ["-archs", executable.path]
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        try process.run()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else { throw InstallError.invalidBundle }
        return String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            .split(whereSeparator: \ .isWhitespace)
            .map(String.init)
    }
}

struct BundleValidator: BundleValidating {
    static let bundleIdentifier = "com.pca.PersonalComputerAgent"
    static let launchAgentName = "com.pca.agentd.plist"

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
        self.expectedTeamIdentifier = expectedTeamIdentifier
            ?? ProcessInfo.processInfo.environment["PCA_EXPECTED_TEAM_ID"]
            ?? (try? signatureChecker.verifyAndReadTeamIdentifier(of: Bundle.main.bundleURL))
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
              let candidateVersion = info["CFBundleShortVersionString"] as? String,
              Version(candidateVersion).isValid
        else { throw InstallError.invalidBundle }

        let executable = candidate.appendingPathComponent("Contents/MacOS/PersonalComputerAgent")
        let agent = candidate.appendingPathComponent("Contents/Resources/bin/pca-agentd")
        let bridge = candidate.appendingPathComponent("Contents/Resources/bin/PCAPlatformBridge")
        let launchAgent = candidate.appendingPathComponent("Contents/Library/LaunchAgents/\(Self.launchAgentName)")
        for binary in [executable, agent, bridge] {
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
        for signedTarget in [candidate, agent, bridge] {
            guard try signatureChecker.verifyAndReadTeamIdentifier(of: signedTarget) == expectedTeamIdentifier else {
                throw InstallError.invalidBundle
            }
        }
        return ValidatedBundle(version: candidateVersion, previousVersion: previousVersion)
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
