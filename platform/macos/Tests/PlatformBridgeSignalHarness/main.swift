import Darwin
import Dispatch
import Foundation
@testable import PlatformBridge

private struct HarnessCredentialProvider: BridgeCredentialProviding {
    func loadSecret() throws -> Data? {
        Data(repeating: 0x5a, count: 32)
    }
}

private func makeServer(socketPath: String, runRootPath: String) -> BridgeServer {
    BridgeServer(
        socketURL: URL(fileURLWithPath: socketPath),
        pathValidator: SocketPathValidator(
            approvedRunRoot: URL(fileURLWithPath: runRootPath, isDirectory: true)
        ),
        handshakeHandler: HandshakeHandler(bridgeVersion: "signal-harness"),
        credentialProvider: HarnessCredentialProvider()
    )
}

private func createReadyHook(at path: String) -> Bool {
    FileManager.default.createFile(
        atPath: path,
        contents: Data("ready".utf8)
    )
}

private func waitForAcceptedSignal(_ signalRuntime: TerminationSignalRuntime) {
    while !signalRuntime.terminationAccepted() {
        usleep(1_000)
    }
}

do {
    let signalRuntime = try TerminationSignalRuntime.install()
    if CommandLine.arguments.count == 3,
       CommandLine.arguments[1] == "--invalid-socket" {
        let invalidSocketPath = CommandLine.arguments[2]
        Task {
            exit(await PlatformBridgeExecutable.run(
                arguments: ["PCAPlatformBridge", "--socket", invalidSocketPath],
                signalRuntime: signalRuntime
            ))
        }
    } else if CommandLine.arguments.count == 6,
              CommandLine.arguments[1] == "--invalid-socket",
              CommandLine.arguments[3] == "--ready-hook",
              CommandLine.arguments[5] == "--await-signal" {
        guard createReadyHook(at: CommandLine.arguments[4]) else { exit(3) }
        waitForAcceptedSignal(signalRuntime)
        let invalidSocketPath = CommandLine.arguments[2]
        Task {
            exit(await PlatformBridgeExecutable.run(
                arguments: ["PCAPlatformBridge", "--socket", invalidSocketPath],
                signalRuntime: signalRuntime
            ))
        }
    } else if CommandLine.arguments.count == 8,
              CommandLine.arguments[1] == "--socket",
              CommandLine.arguments[3] == "--run-root",
              CommandLine.arguments[5] == "--ready-hook",
              CommandLine.arguments[7] == "--await-signal-before-start" {
        guard createReadyHook(at: CommandLine.arguments[6]) else { exit(3) }
        waitForAcceptedSignal(signalRuntime)
        let server = makeServer(
            socketPath: CommandLine.arguments[2],
            runRootPath: CommandLine.arguments[4]
        )
        Task {
            exit(await PlatformBridgeExecutable.run(
                server: server,
                signalRuntime: signalRuntime
            ))
        }
    } else if CommandLine.arguments.count == 8,
              CommandLine.arguments[1] == "--socket",
              CommandLine.arguments[3] == "--run-root",
              CommandLine.arguments[5] == "--ready-hook",
              CommandLine.arguments[7] == "--ready-after-start" {
        let server = makeServer(
            socketPath: CommandLine.arguments[2],
            runRootPath: CommandLine.arguments[4]
        )
        Task {
            if let failureCode = await PlatformBridgeExecutable.start(
                server: server,
                signalRuntime: signalRuntime
            ) {
                exit(failureCode)
            }
            signalRuntime.startReader()
            guard createReadyHook(at: CommandLine.arguments[6]) else {
                try? await server.shutdown()
                exit(3)
            }
            exit(await PlatformBridgeExecutable.serveStarted(server: server))
        }
    } else {
        exit(2)
    }
    dispatchMain()
} catch {
    exit(5)
}
