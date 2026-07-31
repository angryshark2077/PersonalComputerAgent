import Foundation

@MainActor
final class PairingCoordinator {
    private let agent: any PairingAgentHandingOff
    private let browser: any PairingBrowserOpening
    private let callbackServerFactory: @MainActor @Sendable () throws -> PairingCallbackServer
    private var callbackServer: PairingCallbackServer?
    private var expectedState: String?
    private var consumed = false

    init(
        agent: any PairingAgentHandingOff,
        browser: any PairingBrowserOpening = SystemPairingBrowser(),
        callbackServerFactory: @escaping @MainActor @Sendable () throws -> PairingCallbackServer = { try PairingCallbackServer() }
    ) {
        self.agent = agent
        self.browser = browser
        self.callbackServerFactory = callbackServerFactory
    }

    static func fake(state: String) -> PairingCoordinator {
        PairingCoordinator(agent: UnavailablePairingAgentBridge(), callbackServerFactory: {
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
        let server = try callbackServerFactory()
        callbackServer = server
        let callbackURI = try await server.start()

        do {
            let handoff = PairingStartHandoff(callbackURI: callbackURI)
            let session = try await agent.beginPairing(handoff)
            activeSessionID = session.sessionID
            expectedState = session.callbackState
            try server.setExpectedState(session.callbackState)
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
                authorizationCode: acceptedCallback.authorizationCode
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
