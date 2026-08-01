import BridgeProtocol
import Darwin
import Foundation
@testable import PlatformBridge
import XCTest

final class HandshakeTests: XCTestCase {
    private let secret = Data(repeating: 0x5a, count: 32)
    private let nonce = Data(repeating: 0x11, count: 32)

    func testProofMatchesRustTranscriptGoldenVector() throws {
        let proof = try BridgeProof.make(
            secret: secret,
            nonce: nonce,
            protocolVersion: 0x0102_0304,
            agentVersion: "v1.β"
        )

        XCTAssertEqual(proof, "ZzHI3PgX7xuVBQpbtbnGsqP8Tvcu9WBICkuw1YUGwmc=")
        XCTAssertTrue(BridgeProof.verify(
            proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 0x0102_0304,
            agentVersion: "v1.β"
        ))
    }

    func testInvalidProofIsRejectedBySharedVerifier() {
        XCTAssertFalse(BridgeProof.verify(
            Data(repeating: 0, count: 32).base64EncodedString(),
            secret: secret,
            nonce: nonce,
            protocolVersion: 0x0102_0304,
            agentVersion: "v1.β"
        ))
    }

    func testChallengeProducesStrictCorrelatedAuthenticatedResponse() throws {
        let requestID = UUID(uuidString: "018f3f4a-2d9b-7d21-a310-2c49d9b43c12")!
        let challenge = try challengeData(protocolVersion: 1, requestID: requestID)
        let handler = HandshakeHandler(bridgeVersion: "0.0.0-s1a")

        let result = try handler.respond(to: challenge, secret: secret)
        let response = try JSONDecoder().decode(BridgeEnvelope.self, from: result.responseJSON)
        let payloadData = try JSONEncoder().encode(response.payload)
        let payload = try JSONDecoder().decode(HandshakeResponse.self, from: payloadData)

        XCTAssertTrue(result.protocolCompatible)
        XCTAssertEqual(response.protocolVersion, 1)
        XCTAssertEqual(response.requestID, requestID)
        XCTAssertEqual(response.messageKind, .response)
        XCTAssertEqual(response.capability, "bridge.handshake")
        XCTAssertEqual(response.deadlineMilliseconds, 1_000)
        XCTAssertNil(response.error)
        XCTAssertEqual(payload.phase, .response)
        XCTAssertEqual(payload.nonce, nonce.base64EncodedString())
        XCTAssertEqual(payload.bridgeVersion, "0.0.0-s1a")
        XCTAssertTrue(BridgeProof.verify(
            payload.proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 1,
            agentVersion: "v1.β"
        ))
    }

    func testUnsupportedInboundVersionReturnsAuthenticatedV1ThenRequiresClose() throws {
        let challenge = try challengeData(protocolVersion: 999, requestID: UUID())
        let handler = HandshakeHandler(bridgeVersion: "0.0.0-s1a")

        let result = try handler.respond(to: challenge, secret: secret)
        let response = try JSONDecoder().decode(BridgeEnvelope.self, from: result.responseJSON)
        let payloadData = try JSONEncoder().encode(response.payload)
        let payload = try JSONDecoder().decode(HandshakeResponse.self, from: payloadData)

        XCTAssertFalse(result.protocolCompatible)
        XCTAssertEqual(response.protocolVersion, 1)
        XCTAssertTrue(BridgeProof.verify(
            payload.proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 1,
            agentVersion: "v1.β"
        ))
        XCTAssertFalse(BridgeProof.verify(
            payload.proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: 999,
            agentVersion: "v1.β"
        ))
    }

    func testUnknownAndDuplicateHandshakeFieldsAreRejected() throws {
        let requestID = UUID().uuidString.lowercased()
        let nonce = nonce.base64EncodedString()
        let handler = HandshakeHandler(bridgeVersion: "0.0.0-s1a")
        let unknown = Data("""
        {"protocol_version":1,"request_id":"\(requestID)","message_kind":"request","capability":"bridge.handshake","deadline_ms":1000,"payload":{"phase":"challenge","nonce":"\(nonce)","agent_version":"v1.β","extra":true}}
        """.utf8)
        let duplicate = Data("""
        {"protocol_version":1,"protocol_version":1,"request_id":"\(requestID)","message_kind":"request","capability":"bridge.handshake","deadline_ms":1000,"payload":{"phase":"challenge","nonce":"\(nonce)","agent_version":"v1.β"}}
        """.utf8)

        XCTAssertThrowsError(try handler.respond(to: unknown, secret: secret))
        XCTAssertThrowsError(try handler.respond(to: duplicate, secret: secret))
    }

    func testInvalidEnvelopeFieldsNonceAndCredentialAreRejected() throws {
        let valid = try challengeData(protocolVersion: 1, requestID: UUID())
        let handler = HandshakeHandler(bridgeVersion: "0.0.0-s1a")
        let wrongKind = try replacing(valid, key: "message_kind", with: "event")
        let zeroDeadline = try replacing(valid, key: "deadline_ms", with: 0)
        let badNonce = try replacingPayload(valid, key: "nonce", with: "AA==")
        let emptyAgent = try replacingPayload(valid, key: "agent_version", with: "")

        for invalid in [wrongKind, zeroDeadline, badNonce, emptyAgent] {
            XCTAssertThrowsError(try handler.respond(to: invalid, secret: secret))
        }
        XCTAssertThrowsError(try handler.respond(to: valid, secret: Data(repeating: 1, count: 31)))
    }

    func testSocketPathValidatorAllowsOnlyDirectChildAndRejectsUnsafeEntries() throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-bt-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let validator = SocketPathValidator(approvedRunRoot: runRoot)
        try validator.prepareRunDirectory()
        var runInfo = stat()
        XCTAssertEqual(lstat(runRoot.path, &runInfo), 0)
        XCTAssertEqual(runInfo.st_mode & mode_t(S_IFMT), mode_t(S_IFDIR))
        XCTAssertEqual(runInfo.st_mode & 0o777, 0o700)
        XCTAssertEqual(runInfo.st_uid, geteuid())

        let socket = runRoot.appendingPathComponent("bridge.sock")
        XCTAssertNoThrow(try validator.validate(socketURL: socket))
        XCTAssertThrowsError(try validator.validate(socketURL: runRoot.appendingPathComponent("nested/bridge.sock")))
        XCTAssertThrowsError(try validator.validate(socketURL: parent.appendingPathComponent("bridge.sock")))

        let regular = runRoot.appendingPathComponent("regular")
        XCTAssertTrue(FileManager.default.createFile(atPath: regular.path, contents: Data()))
        XCTAssertThrowsError(try validator.removeStaleSocketIfSafe(at: regular))
        XCTAssertTrue(FileManager.default.fileExists(atPath: regular.path))
        XCTAssertTrue(try quarantineArtifacts(in: runRoot).isEmpty)

        let target = runRoot.appendingPathComponent("target")
        XCTAssertTrue(FileManager.default.createFile(atPath: target.path, contents: Data()))
        let link = runRoot.appendingPathComponent("link")
        XCTAssertEqual(symlink(target.path, link.path), 0)
        XCTAssertThrowsError(try validator.removeStaleSocketIfSafe(at: link))
        XCTAssertEqual(lstatExists(link.path), 0)
        XCTAssertTrue(try quarantineArtifacts(in: runRoot).isEmpty)

        let staleSocket = runRoot.appendingPathComponent("stale.sock")
        let staleDescriptor = try bindUnixSocket(at: staleSocket.path)
        Darwin.close(staleDescriptor)
        try validator.removeStaleSocketIfSafe(at: staleSocket)
        XCTAssertNotEqual(lstatExists(staleSocket.path), 0)
        XCTAssertTrue(try quarantineArtifacts(in: runRoot).isEmpty)
    }

    func testServerBindsPrivateSocketAndRemovesItOnShutdown() async throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-bs-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let validator = SocketPathValidator(approvedRunRoot: runRoot)
        let server = BridgeServer(
            socketURL: socket,
            pathValidator: validator,
            handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
            credentialProvider: FixedCredentialProvider(secret: secret)
        )

        try await server.start()
        var info = stat()
        XCTAssertEqual(lstat(socket.path, &info), 0)
        XCTAssertEqual(info.st_mode & mode_t(S_IFMT), mode_t(S_IFSOCK))
        XCTAssertEqual(info.st_mode & 0o777, 0o600)
        XCTAssertEqual(info.st_uid, geteuid())

        try await server.shutdown()
        XCTAssertNotEqual(lstat(socket.path, &info), 0)
    }

    func testShutdownRequestedBeforeStartupIsReportedAsGracefulTermination() async throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-sig-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let server = BridgeServer(
            socketURL: socket,
            pathValidator: SocketPathValidator(approvedRunRoot: runRoot),
            handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
            credentialProvider: FixedCredentialProvider(secret: secret)
        )

        try await server.shutdown()
        do {
            try await server.start()
            XCTFail("startup after termination request must not bind")
        } catch {
            XCTAssertEqual(error as? BridgeServerError, .shutdownRequested)
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: socket.path))
    }

    func testProcessLevelStartupSignalsExitZeroWithoutSocketArtifacts() async throws {
        let productsURL = Bundle(for: Self.self).bundleURL.deletingLastPathComponent()
        let harnessURL = productsURL.appendingPathComponent("PlatformBridgeSignalHarness")
        let productURL = productsURL.appendingPathComponent("PCAPlatformBridge")
        XCTAssertTrue(FileManager.default.isExecutableFile(atPath: harnessURL.path))
        XCTAssertTrue(FileManager.default.isExecutableFile(atPath: productURL.path))

        let signalCases: [(label: String, signals: [Int32])] = [
            ("sigterm", [SIGTERM]),
            ("sigint", [SIGINT]),
            ("repeated", [SIGTERM, SIGINT]),
        ]
        for signalCase in signalCases {
            let parent = URL(
                fileURLWithPath: "/tmp/pca-signal-\(signalCase.label)-\(UUID().uuidString.prefix(8))",
                isDirectory: true
            )
            try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
            defer { try? FileManager.default.removeItem(at: parent) }
            let invalidSocket = parent.appendingPathComponent("invalid.sock")
            let readyHook = parent.appendingPathComponent("startup-ready")
            let process = Process()
            process.executableURL = harnessURL
            process.arguments = [
                "--invalid-socket", invalidSocket.path,
                "--ready-hook", readyHook.path,
                "--await-signal",
            ]
            process.standardOutput = FileHandle.nullDevice
            process.standardError = FileHandle.nullDevice
            try process.run()

            let readyDeadline = ContinuousClock.now.advanced(by: .seconds(2))
            while !FileManager.default.fileExists(atPath: readyHook.path),
                  process.isRunning,
                  ContinuousClock.now < readyDeadline {
                try await Task.sleep(for: .milliseconds(5))
            }
            guard FileManager.default.fileExists(atPath: readyHook.path) else {
                if process.isRunning { kill(process.processIdentifier, SIGKILL) }
                process.waitUntilExit()
                XCTFail("signal harness never reached the installed-relay startup hook")
                continue
            }

            for terminationSignal in signalCase.signals {
                XCTAssertEqual(kill(process.processIdentifier, terminationSignal), 0)
            }
            let exitDeadline = ContinuousClock.now.advanced(by: .seconds(2))
            while process.isRunning, ContinuousClock.now < exitDeadline {
                try await Task.sleep(for: .milliseconds(5))
            }
            if process.isRunning {
                kill(process.processIdentifier, SIGKILL)
                process.waitUntilExit()
                XCTFail("signal harness exceeded its hard exit deadline")
                continue
            }
            process.waitUntilExit()

            XCTAssertEqual(process.terminationReason, .exit)
            XCTAssertEqual(process.terminationStatus, 0)
            XCTAssertFalse(FileManager.default.fileExists(atPath: invalidSocket.path))
            let entries = try FileManager.default.contentsOfDirectory(atPath: parent.path)
            XCTAssertFalse(entries.contains { $0.hasPrefix(".pca-quarantine-") })
        }

        let invalidParent = URL(
            fileURLWithPath: "/tmp/pca-signal-invalid-\(UUID().uuidString.prefix(8))",
            isDirectory: true
        )
        try FileManager.default.createDirectory(at: invalidParent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: invalidParent) }
        let invalidProcess = Process()
        invalidProcess.executableURL = harnessURL
        invalidProcess.arguments = [
            "--invalid-socket", invalidParent.appendingPathComponent("invalid.sock").path,
        ]
        invalidProcess.standardOutput = FileHandle.nullDevice
        invalidProcess.standardError = FileHandle.nullDevice
        try invalidProcess.run()
        let invalidExitDeadline = ContinuousClock.now.advanced(by: .seconds(2))
        while invalidProcess.isRunning, ContinuousClock.now < invalidExitDeadline {
            try await Task.sleep(for: .milliseconds(5))
        }
        if invalidProcess.isRunning {
            kill(invalidProcess.processIdentifier, SIGKILL)
            invalidProcess.waitUntilExit()
            XCTFail("invalid invocation exceeded its hard exit deadline")
        } else {
            invalidProcess.waitUntilExit()
            XCTAssertEqual(invalidProcess.terminationReason, .exit)
            XCTAssertEqual(invalidProcess.terminationStatus, 1)
        }
    }

    func testSignalDuringDelayedStartIsConsumedAfterRealServerStarts() async throws {
        let harnessURL = Bundle(for: Self.self).bundleURL
            .deletingLastPathComponent()
            .appendingPathComponent("PlatformBridgeSignalHarness")
        let parent = URL(
            fileURLWithPath: "/tmp/pca-signal-delayed-\(UUID().uuidString.prefix(8))",
            isDirectory: true
        )
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let readyHook = parent.appendingPathComponent("startup-ready")
        let process = Process()
        process.executableURL = harnessURL
        process.arguments = [
            "--socket", socket.path,
            "--run-root", runRoot.path,
            "--ready-hook", readyHook.path,
            "--await-signal-before-start",
        ]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()

        guard try await waitForFile(
            readyHook,
            process: process,
            failure: "delayed-start harness was not ready"
        ) else { return }
        XCTAssertEqual(kill(process.processIdentifier, SIGTERM), 0)
        guard try await waitForExit(
            process,
            expectedStatus: 0,
            failure: "delayed-start harness did not exit"
        ) else { return }
        XCTAssertFalse(FileManager.default.fileExists(atPath: socket.path))
        XCTAssertTrue(try quarantineArtifacts(in: runRoot).isEmpty)
    }

    func testPostStartSignalsCleanlyStopRealServerProcess() async throws {
        let harnessURL = Bundle(for: Self.self).bundleURL
            .deletingLastPathComponent()
            .appendingPathComponent("PlatformBridgeSignalHarness")
        let signalCases: [(label: String, signals: [Int32], delayMilliseconds: UInt64)] = [
            ("immediate-sigterm", [SIGTERM], 0),
            ("serving-sigint", [SIGINT], 50),
            ("repeated", [SIGTERM, SIGINT], 0),
        ]
        for iteration in 0..<3 {
            for signalCase in signalCases {
                let parent = URL(
                    fileURLWithPath: "/tmp/pca-signal-post-\(iteration)-\(signalCase.label)-\(UUID().uuidString.prefix(8))",
                    isDirectory: true
                )
                try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
                defer { try? FileManager.default.removeItem(at: parent) }
                let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
                let socket = runRoot.appendingPathComponent("bridge.sock")
                let readyHook = parent.appendingPathComponent("server-ready")
                let process = Process()
                process.executableURL = harnessURL
                process.arguments = [
                    "--socket", socket.path,
                    "--run-root", runRoot.path,
                    "--ready-hook", readyHook.path,
                    "--ready-after-start",
                ]
                process.standardOutput = FileHandle.nullDevice
                process.standardError = FileHandle.nullDevice
                try process.run()

                guard try await waitForFile(
                    readyHook,
                    process: process,
                    failure: "post-start harness was not ready"
                ) else { return }
                XCTAssertTrue(FileManager.default.fileExists(atPath: socket.path))
                if signalCase.delayMilliseconds > 0 {
                    try await Task.sleep(for: .milliseconds(signalCase.delayMilliseconds))
                }
                for terminationSignal in signalCase.signals {
                    XCTAssertEqual(kill(process.processIdentifier, terminationSignal), 0)
                }
                guard try await waitForExit(
                    process,
                    expectedStatus: 0,
                    failure: "post-start harness did not exit"
                ) else { return }
                XCTAssertFalse(FileManager.default.fileExists(atPath: socket.path))
                XCTAssertTrue(try quarantineArtifacts(in: runRoot).isEmpty)
            }
        }
    }

    func testCapabilityRequestRejectsIntMaxPlusOneAndUInt64MaxWithoutTrap() throws {
        let tooLarge = UInt64(Int.max) + 1
        for deadline in [BridgeWireLimits.maximumDeadlineMilliseconds + 1, tooLarge, UInt64.max] {
            let request = Data("""
            {"protocol_version":1,"request_id":"\(UUID().uuidString)","message_kind":"request","capability":"system.capabilities","deadline_ms":\(deadline),"payload":{"include_permissions":true}}
            """.utf8)

            XCTAssertThrowsError(try CapabilityRequestHandler.respond(to: request)) { error in
                XCTAssertEqual(error as? BridgeServerError, .invalidRequest)
            }
        }
    }

    func testSlowCredentialLookupTimesOutWithoutBlockingActorShutdown() async throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-slow-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let server = BridgeServer(
            socketURL: socket,
            pathValidator: SocketPathValidator(approvedRunRoot: runRoot),
            handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
            credentialProvider: SlowCredentialProvider(
                delayMicroseconds: 400_000,
                secret: secret
            ),
            credentialTimeoutMilliseconds: 30
        )
        try await server.start()
        let serveTask = Task { try await server.serve() }
        let client = try connectUnixSocket(at: socket.path)
        defer { Darwin.close(client) }
        try writeAll(try FrameCodec.encode(challengeData(protocolVersion: 1, requestID: UUID())), to: client)
        try await Task.sleep(for: .milliseconds(10))

        let clock = ContinuousClock()
        let start = clock.now
        try await server.shutdown()
        let elapsed = start.duration(to: clock.now)
        serveTask.cancel()
        _ = try? await serveTask.value

        XCTAssertLessThan(elapsed, .milliseconds(150))
        XCTAssertFalse(FileManager.default.fileExists(atPath: socket.path))
    }

    func testGracefulCleanupPreservesReplacementAndSurfacesIdentityMismatch() async throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-repl-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let server = BridgeServer(
            socketURL: socket,
            pathValidator: SocketPathValidator(approvedRunRoot: runRoot),
            handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
            credentialProvider: FixedCredentialProvider(secret: secret)
        )
        try await server.start()
        XCTAssertEqual(unlink(socket.path), 0)
        let replacement = Data("preserve-me".utf8)
        XCTAssertTrue(FileManager.default.createFile(atPath: socket.path, contents: replacement))

        do {
            try await server.shutdown()
            XCTFail("replacement identity must surface cleanup failure")
        } catch {
            XCTAssertEqual(error as? BridgeServerError, .socketIdentityMismatch)
        }
        XCTAssertEqual(try Data(contentsOf: socket), replacement)
        XCTAssertTrue(try quarantineArtifacts(in: runRoot).isEmpty)
    }

    func testSlowCredentialLookupHitsRealTimeoutAndClosesUnauthenticatedPeer() async throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-auth-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let server = BridgeServer(
            socketURL: socket,
            pathValidator: SocketPathValidator(approvedRunRoot: runRoot),
            handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
            credentialProvider: SlowCredentialProvider(
                delayMicroseconds: 400_000,
                secret: secret
            ),
            credentialTimeoutMilliseconds: 30
        )
        try await server.start()
        let serveTask = Task { try await server.serve() }
        let client = try connectUnixSocket(at: socket.path)
        try writeFragmented(
            try FrameCodec.encode(challengeData(protocolVersion: 1, requestID: UUID())),
            to: client
        )

        let clock = ContinuousClock()
        let start = clock.now
        let closed = try await waitForEOF(from: client, timeoutMilliseconds: 200)
        XCTAssertTrue(closed)
        XCTAssertLessThan(start.duration(to: clock.now), .milliseconds(180))
        Darwin.close(client)

        try await server.shutdown()
        try await serveTask.value
    }

    func testRealServerEndToEndStrictHandshakeRequestsAndConnectionLifecycle() async throws {
        let parent = URL(fileURLWithPath: "/tmp/pca-e2e-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: parent) }
        let runRoot = parent.appendingPathComponent("Run", isDirectory: true)
        let socket = runRoot.appendingPathComponent("bridge.sock")
        let server = BridgeServer(
            socketURL: socket,
            pathValidator: SocketPathValidator(approvedRunRoot: runRoot),
            handshakeHandler: HandshakeHandler(bridgeVersion: "0.0.0-s1a"),
            credentialProvider: FixedCredentialProvider(secret: secret),
            idleTimeoutMilliseconds: 60_000
        )
        try await server.start()
        let serveTask = Task { try await server.serve() }

        let first = try connectUnixSocket(at: socket.path)
        let firstRequestID = UUID()
        let capabilityID = UUID()
        var backToBack = try FrameCodec.encode(challengeData(protocolVersion: 1, requestID: firstRequestID))
        backToBack.append(try FrameCodec.encode(capabilityRequest(requestID: capabilityID, deadline: 1_000)))
        try writeFragmented(backToBack, to: first)

        let handshakeJSON = try await readFrame(from: first, oneByteAtATime: true)
        try assertAuthenticatedHandshake(
            handshakeJSON,
            requestID: firstRequestID,
            expectedCompatibleVersion: 1
        )
        let capabilityJSON = try await readFrame(from: first, oneByteAtATime: true)
        let capability = try JSONDecoder().decode(BridgeEnvelope.self, from: capabilityJSON)
        XCTAssertEqual(capability.requestID, capabilityID)
        XCTAssertEqual(capability.messageKind, .response)
        XCTAssertEqual(capability.capability, "system.capabilities")
        XCTAssertEqual(capability.deadlineMilliseconds, 1_000)
        XCTAssertEqual(capability.payload, ["screen_capture": .string("available")])

        let second = try connectUnixSocket(at: socket.path)
        let secondID = UUID()
        try writeFragmented(
            try FrameCodec.encode(challengeData(protocolVersion: 1, requestID: secondID)),
            to: second
        )
        do {
            _ = try await readFrame(from: second, timeoutMilliseconds: 50)
            XCTFail("second client must not authenticate while the first remains active")
        } catch TestSocketError.timeout {
            // Expected: the listener serializes authenticated clients.
        }
        Darwin.close(first)
        let secondHandshake = try await readFrame(from: second)
        try assertAuthenticatedHandshake(
            secondHandshake,
            requestID: secondID,
            expectedCompatibleVersion: 1
        )
        Darwin.close(second)

        let incompatible = try connectUnixSocket(at: socket.path)
        let incompatibleID = UUID()
        try writeFragmented(
            try FrameCodec.encode(challengeData(protocolVersion: 999, requestID: incompatibleID)),
            to: incompatible
        )
        let incompatibleResponse = try await readFrame(from: incompatible)
        try assertAuthenticatedHandshake(
            incompatibleResponse,
            requestID: incompatibleID,
            expectedCompatibleVersion: 1
        )
        let incompatibleClosed = try await waitForEOF(from: incompatible)
        XCTAssertTrue(incompatibleClosed)
        Darwin.close(incompatible)

        let malformed = try connectUnixSocket(at: socket.path)
        try writeFragmented(try FrameCodec.encode(Data("{}".utf8)), to: malformed)
        let malformedClosed = try await waitForEOF(from: malformed)
        XCTAssertTrue(malformedClosed)
        Darwin.close(malformed)

        for deadline in [UInt64(Int.max) + 1, UInt64.max] {
            let client = try connectUnixSocket(at: socket.path)
            let requestID = UUID()
            try writeFragmented(
                try FrameCodec.encode(challengeData(protocolVersion: 1, requestID: requestID)),
                to: client
            )
            _ = try await readFrame(from: client)
            try writeFragmented(
                try FrameCodec.encode(capabilityRequest(requestID: UUID(), deadline: deadline)),
                to: client
            )
            let invalidDeadlineClosed = try await waitForEOF(from: client)
            XCTAssertTrue(invalidDeadlineClosed)
            Darwin.close(client)
        }

        let oversized = try connectUnixSocket(at: socket.path)
        var oversizedLength = UInt32(FrameCodec.maximumFrameBytes + 1).bigEndian
        try writeFragmented(withUnsafeBytes(of: &oversizedLength) { Data($0) }, to: oversized)
        let oversizedClosed = try await waitForEOF(from: oversized)
        XCTAssertTrue(oversizedClosed)
        Darwin.close(oversized)

        try await server.shutdown()
        try await serveTask.value
        XCTAssertFalse(FileManager.default.fileExists(atPath: socket.path))
    }

    private func waitForFile(
        _ file: URL,
        process: Process,
        failure: String
    ) async throws -> Bool {
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        while !FileManager.default.fileExists(atPath: file.path),
              process.isRunning,
              ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(5))
        }
        guard FileManager.default.fileExists(atPath: file.path) else {
            if process.isRunning { kill(process.processIdentifier, SIGKILL) }
            process.waitUntilExit()
            XCTFail(failure)
            return false
        }
        return true
    }

    private func waitForExit(
        _ process: Process,
        expectedStatus: Int32,
        failure: String
    ) async throws -> Bool {
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        while process.isRunning, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(5))
        }
        if process.isRunning {
            kill(process.processIdentifier, SIGKILL)
            process.waitUntilExit()
            XCTFail(failure)
            return false
        }
        process.waitUntilExit()
        XCTAssertEqual(process.terminationReason, .exit, failure)
        XCTAssertEqual(process.terminationStatus, expectedStatus, failure)
        return true
    }

    private func challengeData(protocolVersion: Int, requestID: UUID) throws -> Data {
        let envelope = BridgeEnvelope(
            protocolVersion: protocolVersion,
            requestID: requestID,
            messageKind: .request,
            capability: "bridge.handshake",
            deadlineMilliseconds: 1_000,
            payload: [
                "phase": .string("challenge"),
                "nonce": .string(nonce.base64EncodedString()),
                "agent_version": .string("v1.β"),
            ]
        )
        return try JSONEncoder().encode(envelope)
    }

    private func capabilityRequest(requestID: UUID, deadline: UInt64) throws -> Data {
        Data("""
        {"protocol_version":1,"request_id":"\(requestID.uuidString)","message_kind":"request","capability":"system.capabilities","deadline_ms":\(deadline),"payload":{"include_permissions":true}}
        """.utf8)
    }

    private func assertAuthenticatedHandshake(
        _ data: Data,
        requestID: UUID,
        expectedCompatibleVersion: Int
    ) throws {
        let response = try JSONDecoder().decode(BridgeEnvelope.self, from: data)
        let payloadData = try JSONEncoder().encode(response.payload)
        let payload = try JSONDecoder().decode(HandshakeResponse.self, from: payloadData)
        XCTAssertEqual(response.protocolVersion, expectedCompatibleVersion)
        XCTAssertEqual(response.requestID, requestID)
        XCTAssertEqual(response.messageKind, .response)
        XCTAssertEqual(response.capability, "bridge.handshake")
        XCTAssertTrue(BridgeProof.verify(
            payload.proof,
            secret: secret,
            nonce: nonce,
            protocolVersion: UInt32(expectedCompatibleVersion),
            agentVersion: "v1.β"
        ))
    }

    private func replacing(_ data: Data, key: String, with value: Any) throws -> Data {
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        object[key] = value
        return try JSONSerialization.data(withJSONObject: object)
    }

    private func replacingPayload(_ data: Data, key: String, with value: Any) throws -> Data {
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        var payload = try XCTUnwrap(object["payload"] as? [String: Any])
        payload[key] = value
        object["payload"] = payload
        return try JSONSerialization.data(withJSONObject: object)
    }

    private func lstatExists(_ path: String) -> Int32 {
        var info = stat()
        return lstat(path, &info)
    }

    private func quarantineArtifacts(in runRoot: URL) throws -> [String] {
        try FileManager.default.contentsOfDirectory(atPath: runRoot.path)
            .filter { $0.hasPrefix(".pca-quarantine-") }
    }

    private func connectUnixSocket(at path: String) throws -> Int32 {
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw TestSocketError.operation }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard bytes.count < capacity else {
            Darwin.close(descriptor)
            throw TestSocketError.operation
        }
        withUnsafeMutablePointer(to: &address.sun_path) { pointer in
            pointer.withMemoryRebound(to: CChar.self, capacity: capacity) { target in
                for (index, byte) in bytes.enumerated() { target[index] = CChar(bitPattern: byte) }
                target[bytes.count] = 0
            }
        }
        let length = socklen_t(MemoryLayout.offset(of: \sockaddr_un.sun_path)! + bytes.count + 1)
        address.sun_len = UInt8(length)
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(descriptor, $0, length)
            }
        }
        guard result == 0 else {
            Darwin.close(descriptor)
            throw TestSocketError.operation
        }
        return descriptor
    }

    private func bindUnixSocket(at path: String) throws -> Int32 {
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw TestSocketError.operation }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard bytes.count < capacity else {
            Darwin.close(descriptor)
            throw TestSocketError.operation
        }
        withUnsafeMutablePointer(to: &address.sun_path) { pointer in
            pointer.withMemoryRebound(to: CChar.self, capacity: capacity) { target in
                for (index, byte) in bytes.enumerated() { target[index] = CChar(bitPattern: byte) }
                target[bytes.count] = 0
            }
        }
        let length = socklen_t(MemoryLayout.offset(of: \sockaddr_un.sun_path)! + bytes.count + 1)
        address.sun_len = UInt8(length)
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(descriptor, $0, length)
            }
        }
        guard result == 0 else {
            Darwin.close(descriptor)
            throw TestSocketError.operation
        }
        return descriptor
    }

    private func writeAll(_ data: Data, to descriptor: Int32) throws {
        var offset = 0
        while offset < data.count {
            let count = data.withUnsafeBytes { buffer -> Int in
                guard let base = buffer.baseAddress else { return -1 }
                return Darwin.send(descriptor, base.advanced(by: offset), data.count - offset, 0)
            }
            guard count > 0 else { throw TestSocketError.operation }
            offset += count
        }
    }

    private func writeFragmented(_ data: Data, to descriptor: Int32) throws {
        let fragmentSizes = [1, 2, 3, 5, 8, 13]
        var offset = 0
        var fragmentIndex = 0
        while offset < data.count {
            let length = min(fragmentSizes[fragmentIndex % fragmentSizes.count], data.count - offset)
            try writeAll(Data(data[offset..<(offset + length)]), to: descriptor)
            offset += length
            fragmentIndex += 1
        }
    }

    private func readFrame(
        from descriptor: Int32,
        timeoutMilliseconds: UInt64 = 2_000,
        oneByteAtATime: Bool = false
    ) async throws -> Data {
        var decoder = FrameDecoder()
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(timeoutMilliseconds))
        let capacity = oneByteAtATime ? 1 : 4096
        var bytes = [UInt8](repeating: 0, count: capacity)
        while ContinuousClock.now < deadline {
            let count = Darwin.recv(descriptor, &bytes, bytes.count, MSG_DONTWAIT)
            if count > 0 {
                if let frame = try decoder.append(Data(bytes.prefix(Int(count)))).first {
                    return frame
                }
            } else if count == 0 {
                throw TestSocketError.eof
            } else if errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR {
                throw TestSocketError.operation
            }
            try await Task.sleep(for: .milliseconds(2))
        }
        throw TestSocketError.timeout
    }

    private func waitForEOF(
        from descriptor: Int32,
        timeoutMilliseconds: UInt64 = 1_000
    ) async throws -> Bool {
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(timeoutMilliseconds))
        var byte: UInt8 = 0
        while ContinuousClock.now < deadline {
            let count = Darwin.recv(descriptor, &byte, 1, MSG_DONTWAIT)
            if count == 0 { return true }
            if count < 0, errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR {
                throw TestSocketError.operation
            }
            if count > 0 { return false }
            try await Task.sleep(for: .milliseconds(2))
        }
        throw TestSocketError.timeout
    }
}

private struct FixedCredentialProvider: BridgeCredentialProviding {
    let secret: Data?

    func loadSecret() throws -> Data? {
        secret
    }
}

private struct SlowCredentialProvider: BridgeCredentialProviding {
    let delayMicroseconds: useconds_t
    let secret: Data?

    func loadSecret() throws -> Data? {
        usleep(delayMicroseconds)
        return secret
    }
}

private enum TestSocketError: Error {
    case operation
    case timeout
    case eof
}
