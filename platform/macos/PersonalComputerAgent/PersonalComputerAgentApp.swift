import AppKit
import Darwin
import SwiftUI

final class InstallerApplicationDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

@main
enum PersonalComputerAgentMain {
    static func main() {
        let arguments = Array(CommandLine.arguments.dropFirst())
        if arguments.first == "--uninstall" {
            runUninstall(arguments: Array(arguments.dropFirst()))
            return
        }
        PCAInstallerApplication.main()
    }

    private static func runUninstall(arguments: [String]) {
        let valid = arguments.isEmpty || arguments == ["--delete-data"]
        guard valid else {
            FileHandle.standardError.write(Data("usage: PersonalComputerAgent --uninstall [--delete-data]\n".utf8))
            exit(2)
        }
        Task { @MainActor in
            do {
                let command = UninstallCommand(
                    paths: try InstallPaths.production(),
                    service: ServiceController()
                )
                try await command.execute(deleteData: arguments == ["--delete-data"])
                print("Personal Computer Agent was uninstalled.")
                exit(0)
            } catch let error as InstallError {
                FileHandle.standardError.write(Data("Uninstall failed: \(error.localizedDescription)\n".utf8))
                exit(1)
            } catch {
                FileHandle.standardError.write(Data("Uninstall failed safely. No unrelated data was removed.\n".utf8))
                exit(1)
            }
        }
        RunLoop.main.run()
    }
}

private struct PCAInstallerApplication: App {
    @NSApplicationDelegateAdaptor(InstallerApplicationDelegate.self) private var applicationDelegate
    @StateObject private var model: InstallerViewModel

    init() {
        do {
            let paths = try InstallPaths.production()
            let pairingConfiguration = try PairingIPCConfiguration.production(rootURL: paths.rootURL)
            let coordinator = InstallCoordinator(
                paths: paths,
                validator: BundleValidator(),
                service: ServiceController(),
                health: RuntimeHealthChecker(),
                relauncher: ProcessRelauncher()
            )
            _model = StateObject(
                wrappedValue: InstallerViewModel(
                    coordinator: coordinator,
                    sourceBundle: Bundle.main.bundleURL,
                    automaticallyStart: CommandLine.arguments.dropFirst().first == "--setup-installed",
                    pairingCoordinator: PairingCoordinator(agent: InstalledPairingAgentBridge(
                        configuration: pairingConfiguration
                    )),
                    wechatRepairRunner: ProcessWechatRepairRunner(
                        executableURL: paths.installedWechatRepairExecutableURL
                    )
                )
            )
        } catch {
            _model = StateObject(
                wrappedValue: InstallerViewModel(
                    failureMessage: "The fixed user install path is unavailable.",
                    recoveryAction: "Verify your home Library is accessible, then reopen the installer."
                )
            )
        }
    }

    var body: some Scene {
        WindowGroup { InstallerView(model: model) }
            .windowResizability(.contentSize)
    }
}
