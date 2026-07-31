import Foundation
import Security

@MainActor
final class PairingCoordinator {
    private let agent: any PairingAgentHandingOff
    private let browser: any PairingBrowserOpening
    private let callbackServerFactory: @MainActor @Sendable (String) throws -> PairingCallbackServer
    private var callbackServer: PairingCallbackServer?
    private var expectedState: String?
    private var consumed = false

    init(
        agent: any PairingAgentHandingOff,
        browser: any PairingBrowserOpening = SystemPairingBrowser(),
        callbackServerFactory: @escaping @MainActor @Sendable (String) throws -> PairingCallbackServer = PairingCallbackServer.init
    ) {
        self.agent = agent
        self.browser = browser
        self.callbackServerFactory = callbackServerFactory
    }

    static func fake(state: String) -> PairingCoordinator {
        PairingCoordinator(agent: UnavailablePairingAgentBridge(), callbackServerFactory: { _ in
            throw PairingError.unavailable
        }).configuredForTest(state: state)
    }

    var listenerIsClosed: Bool {
        callbackServer?.isClosed ?? consumed
    }

    func startIfUnpaired() async throws -> PairingResult {
        if try await agent.isPaired() { return .alreadyPaired }
        return try await start()
    }

    func repair() async throws -> PairingResult {
        try await start()
    }

    func accept(_ url: URL) async throws -> PairingCallback {
        defer {
            consumed = true
            closeListener()
        }
        guard !consumed else { throw PairingError.alreadyConsumed }
        guard let callback = PairingCallback.parse(url) else { throw PairingError.invalidCallback }
        guard let expectedState else { throw PairingError.invalidCallback }
        guard callback.state == expectedState else { throw PairingError.stateMismatch }
        consumed = true
        return callback
    }

    func cancel() async {
        let sessionID = activeSessionID
        closeListener()
        if let sessionID { await agent.cancelPairing(sessionID: sessionID) }
    }

    private var activeSessionID: String?

    private func start() async throws -> PairingResult {
        guard callbackServer == nil else { throw PairingError.alreadyConsumed }
        let state = try randomURLSafeValue(byteCount: 32)
        let verifier = try randomURLSafeValue(byteCount: 32)
        let server = try callbackServerFactory(state)
        callbackServer = server
        expectedState = state
        let callbackURI = try await server.start()

        do {
            let handoff = PairingStartHandoff(
                callbackURI: callbackURI,
                callbackState: state
            )
            let session = try await agent.beginPairing(handoff)
            activeSessionID = session.sessionID
            guard session.authorizationURL.scheme == "https", browser.open(session.authorizationURL) else {
                throw PairingError.browserLaunchFailed
            }
            let callback = try await withThrowingTaskGroup(of: URL.self) { group in
                group.addTask { try await server.waitForCallback() }
                group.addTask {
                    try await Task.sleep(for: .seconds(300))
                    throw PairingError.expired
                }
                defer { group.cancelAll() }
                return try await group.next()!
            }
            let acceptedCallback = try await accept(callback)
            let result = try await agent.completePairing(PairingCallbackHandoff(
                sessionID: session.sessionID,
                authorizationCode: acceptedCallback.authorizationCode,
                codeVerifier: verifier
            ))
            activeSessionID = nil
            return result
        } catch {
            await cancel()
            throw error
        }
    }

    private func closeListener() {
        callbackServer?.cancel()
        callbackServer = nil
        expectedState = nil
    }

    private func configuredForTest(state: String) -> PairingCoordinator {
        expectedState = state
        return self
    }
}

private func randomURLSafeValue(byteCount: Int) throws -> String {
    var bytes = [UInt8](repeating: 0, count: byteCount)
    guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
        throw PairingError.unavailable
    }
    return Data(bytes).base64EncodedString()
        .replacingOccurrences(of: "+", with: "-")
        .replacingOccurrences(of: "/", with: "_")
        .replacingOccurrences(of: "=", with: "")
}
