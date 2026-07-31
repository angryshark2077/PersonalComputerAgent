import AppKit
import Foundation

enum PairingError: Error, Equatable, LocalizedError, Sendable {
    case unavailable
    case invalidCallback
    case stateMismatch
    case expired
    case alreadyConsumed
    case browserLaunchFailed
    case agentRejected

    var errorDescription: String? {
        switch self {
        case .unavailable: "The local Agent pairing service is unavailable."
        case .invalidCallback: "The pairing callback was invalid."
        case .stateMismatch: "The pairing callback state did not match."
        case .expired: "The pairing request expired."
        case .alreadyConsumed: "The pairing callback was already used."
        case .browserLaunchFailed: "The system browser could not be opened."
        case .agentRejected: "The local Agent rejected the pairing result."
        }
    }
}

enum PairingResult: Equatable, Sendable {
    case alreadyPaired
    case paired(deviceID: String, workspaceID: String)
}

struct PairingStartHandoff: Equatable, Sendable {
    let callbackURI: URL
    let callbackState: String
}

struct PairingSessionHandoff: Equatable, Sendable {
    let sessionID: String
    let authorizationURL: URL
}

struct PairingCallbackHandoff: Equatable, Sendable {
    let sessionID: String
    let authorizationCode: String
    let codeVerifier: String
}

@MainActor
protocol PairingAgentHandingOff: AnyObject {
    func isPaired() async throws -> Bool
    func beginPairing(_ handoff: PairingStartHandoff) async throws -> PairingSessionHandoff
    func completePairing(_ handoff: PairingCallbackHandoff) async throws -> PairingResult
    func cancelPairing(sessionID: String) async
}

@MainActor
final class UnavailablePairingAgentBridge: PairingAgentHandingOff {
    func isPaired() async throws -> Bool { throw PairingError.unavailable }

    func beginPairing(_: PairingStartHandoff) async throws -> PairingSessionHandoff {
        throw PairingError.unavailable
    }

    func completePairing(_: PairingCallbackHandoff) async throws -> PairingResult {
        throw PairingError.unavailable
    }

    func cancelPairing(sessionID _: String) async {}
}

@MainActor
protocol PairingBrowserOpening: AnyObject {
    func open(_ url: URL) -> Bool
}

@MainActor
final class SystemPairingBrowser: PairingBrowserOpening {
    func open(_ url: URL) -> Bool {
        NSWorkspace.shared.open(url)
    }
}
