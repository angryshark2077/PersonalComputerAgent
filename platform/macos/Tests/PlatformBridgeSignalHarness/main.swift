import Darwin
import Dispatch
import Foundation
@testable import PlatformBridge

guard CommandLine.arguments.count == 3 || CommandLine.arguments.count == 6,
      CommandLine.arguments[1] == "--invalid-socket" else {
    exit(2)
}

let invalidSocketPath = CommandLine.arguments[2]
let waitsForSignal = CommandLine.arguments.count == 6
if waitsForSignal {
    guard CommandLine.arguments[3] == "--ready-hook",
          CommandLine.arguments[5] == "--await-signal" else {
        exit(2)
    }
}

do {
    let signalRuntime = try TerminationSignalRuntime.install()
    if waitsForSignal {
        let readyHook = URL(fileURLWithPath: CommandLine.arguments[4])
        guard FileManager.default.createFile(atPath: readyHook.path, contents: Data("ready".utf8)) else {
            exit(3)
        }
        while !signalRuntime.terminationAcceptedOrPending() {
            usleep(1_000)
        }
    }
    Task {
        let code = await PlatformBridgeExecutable.run(
            arguments: ["PCAPlatformBridge", "--socket", invalidSocketPath],
            signalRuntime: signalRuntime
        )
        exit(code)
    }
    dispatchMain()
} catch {
    exit(5)
}
