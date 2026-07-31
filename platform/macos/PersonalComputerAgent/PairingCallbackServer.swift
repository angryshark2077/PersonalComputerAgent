import Foundation
import Network

@MainActor
final class PairingCallbackServer {
    private static let callbackPath = "/pca/pair/callback"
    private let expectedState: String
    private let listener: NWListener
    private var startContinuation: CheckedContinuation<URL, Error>?
    private var continuation: CheckedContinuation<URL, Error>?
    private var terminalResult: Result<URL, Error>?
    private(set) var isClosed = false

    init(expectedState: String) throws {
        self.expectedState = expectedState
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
        listener = try NWListener(using: parameters)
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else {
                connection.cancel()
                return
            }
            Task { @MainActor in self.receive(connection) }
        }
        listener.stateUpdateHandler = { [weak self] state in
            Task { @MainActor in self?.handle(state) }
        }
    }

    func start() async throws -> URL {
        guard !isClosed else { throw PairingError.alreadyConsumed }
        listener.start(queue: .main)
        return try await withCheckedThrowingContinuation { continuation in
            startContinuation = continuation
        }
    }

    func waitForCallback() async throws -> URL {
        if let terminalResult { return try terminalResult.get() }
        guard !isClosed else { throw PairingError.alreadyConsumed }
        return try await withCheckedThrowingContinuation { continuation in
            self.continuation = continuation
        }
    }

    func cancel() {
        close(with: PairingError.expired)
    }

    private func receive(_ connection: NWConnection) {
        guard connection.endpoint.isLoopbackEndpoint else {
            respond(connection, status: "400 Bad Request", body: "Invalid callback.")
            close(with: PairingError.invalidCallback)
            return
        }
        connection.start(queue: .main)
        connection.receive(minimumIncompleteLength: 1, maximumLength: 8_192) { [weak self] data, _, _, _ in
            Task { @MainActor in
                guard let self else {
                    connection.cancel()
                    return
                }
                guard let url = Self.callbackURL(from: data), self.validate(url) else {
                    self.respond(connection, status: "400 Bad Request", body: "Invalid callback.")
                    self.close(with: PairingError.invalidCallback)
                    return
                }
                self.respond(connection, status: "200 OK", body: "Pairing complete. You can return to Personal Computer Agent.")
                self.close(with: url)
            }
        }
    }

    private func handle(_ state: NWListener.State) {
        switch state {
        case .ready:
            guard let port = listener.port,
                  let url = URL(string: "http://127.0.0.1:\(port.rawValue)\(Self.callbackPath)")
            else {
                close(with: PairingError.unavailable)
                return
            }
            startContinuation?.resume(returning: url)
            startContinuation = nil
        case .failed:
            close(with: PairingError.unavailable)
        default:
            break
        }
    }

    private func validate(_ url: URL) -> Bool {
        url.scheme == "http"
            && url.host == "127.0.0.1"
            && url.path == Self.callbackPath
            && url.queryParameters["code"]?.isEmpty == false
            && url.queryParameters["state"] == expectedState
    }

    private func respond(_ connection: NWConnection, status: String, body: String) {
        let response = "HTTP/1.1 \(status)\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: \(body.utf8.count)\r\nConnection: close\r\n\r\n\(body)"
        connection.send(content: Data(response.utf8), completion: .contentProcessed { _ in connection.cancel() })
    }

    private func close(with result: Result<URL, Error>) {
        guard !isClosed else { return }
        isClosed = true
        terminalResult = result
        listener.cancel()
        let startContinuation = startContinuation
        self.startContinuation = nil
        startContinuation?.resume(with: result)
        let continuation = continuation
        self.continuation = nil
        continuation?.resume(with: result)
    }

    private func close(with error: Error) {
        close(with: .failure(error))
    }

    private func close(with url: URL) {
        close(with: .success(url))
    }

    private static func callbackURL(from data: Data?) -> URL? {
        guard let data,
              let request = String(data: data, encoding: .utf8),
              let requestLine = request.split(separator: "\r\n", maxSplits: 1).first
        else { return nil }
        let components = requestLine.split(separator: " ")
        guard components.count == 3, components[0] == "GET", components[2].hasPrefix("HTTP/") else {
            return nil
        }
        return URL(string: "http://127.0.0.1\(components[1])")
    }
}

private extension NWEndpoint {
    var isLoopbackEndpoint: Bool {
        guard case let .hostPort(host, _) = self else { return false }
        return host.debugDescription == "127.0.0.1"
    }
}

private extension URL {
    var queryParameters: [String: String] {
        Dictionary(uniqueKeysWithValues: (URLComponents(url: self, resolvingAgainstBaseURL: false)?.queryItems ?? [])
            .compactMap { item in item.value.map { (item.name, $0) } })
    }
}
