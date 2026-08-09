import SwiftUI

struct InstallerView: View {
    @ObservedObject var model: InstallerViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Personal Computer Agent")
                .font(.largeTitle.bold())
            Text("Installs a user-level background runtime. Program files go in Application Support; persistent local data stays separate across updates.")
                .foregroundStyle(.secondary)

            stateContent
            Spacer(minLength: 8)

            if canInstall {
                Button("Install and Start") { model.installAndStart() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
            }
            if canRepairPairing {
                Button("Retry Pairing") { model.repairPairing() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
            }
            if model.isPairing {
                Button("Cancel Pairing") { model.cancelPairing() }
                    .buttonStyle(.bordered)
            }
            if canRetryWechatAuthorization {
                Button("Retry Administrator Authorization") {
                    model.retryAutomaticWechatRecoveryAuthorization()
                }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
            }
        }
        .padding(28)
        .frame(width: 520, height: 310)
        .onAppear { model.startIfRequested() }
    }

    @ViewBuilder
    private var stateContent: some View {
        switch model.state {
        case .ready:
            Label("Ready to install without administrator access", systemImage: "checkmark.shield")
        case .copying:
            progress("Copying the signed app…")
        case .validating:
            progress("Validating bundle, architecture, and signatures…")
        case .waitingWechatAppDataAccess:
            VStack(alignment: .leading, spacing: 8) {
                progress("Allow access to WeChat data when macOS asks…")
                Text("This read-only check completes Other App Data authorization before WeChat is opened again.")
                    .foregroundStyle(.secondary)
            }
        case .waitingLocationAccess:
            progress("Allow Location access once so Wi-Fi SSID and BSSID remain available after restarts.")
        case .waitingScreenCaptureAccess:
            progress("Allow Screen Recording once so Dashboard screenshots remain available after restarts.")
        case .waitingPhotosAccess:
            progress("Allow Photos access once so recent and future original photos and videos can sync after restarts.")
        case .migratingKeychainAccess:
            progress("Approve the macOS Keychain prompts to preserve pairing and WeChat credentials across this signing change.")
        case .waitingApproval:
            progress("Approve Personal Computer Agent in System Settings > General > Login Items.")
        case .starting:
            progress("Starting the local runtime…")
        case .pairing:
            progress("Complete the secure pairing flow in your browser…")
        case .preparingAutomaticWechatRecovery:
            VStack(alignment: .leading, spacing: 8) {
                progress("Approve one administrator request while WeChat remains closed…")
                Text("This prepares a one-time background recovery. Opening WeChat later will not show another prompt.")
                    .foregroundStyle(.secondary)
            }
        case .automaticWechatRecoveryPrepared:
            VStack(alignment: .leading, spacing: 8) {
                Label("Automatic WeChat recovery is ready", systemImage: "checkmark.circle.fill")
                    .foregroundStyle(.green)
                Text("Open and log in to the official WeChat normally. PCA will capture and validate the key in the background without another prompt.")
                    .foregroundStyle(.secondary)
            }
        case let .automaticWechatRecoveryDeferred(message):
            VStack(alignment: .leading, spacing: 8) {
                Label("Installed and running", systemImage: "checkmark.circle.fill")
                    .foregroundStyle(.green)
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                Text("WeChat recovery is deferred. Keep WeChat closed, disable SIP in macOS Recovery, restart, then retry here.")
                    .foregroundStyle(.secondary)
            }
        case let .automaticWechatRecoveryFailed(message):
            VStack(alignment: .leading, spacing: 8) {
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
                Text("Keep WeChat closed, verify SIP is disabled, then retry the one-time administrator authorization.")
                    .foregroundStyle(.secondary)
            }
        case let .repair(message):
            VStack(alignment: .leading, spacing: 8) {
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
                Text("Retry pairing after the local Agent is available. Existing device credentials were not changed.")
                    .foregroundStyle(.secondary)
            }
        case .success:
            Label("Installed and running", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
        case let .failed(message, recoveryAction):
            VStack(alignment: .leading, spacing: 8) {
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
                Text(recoveryAction).foregroundStyle(.secondary)
            }
        }
    }

    private var canInstall: Bool {
        guard model.installationAvailable else { return false }
        return switch model.state {
        case .ready, .failed: true
        default: false
        }
    }

    private var canRepairPairing: Bool {
        if case .repair = model.state { return !model.isPairing }
        return false
    }

    private var canRetryWechatAuthorization: Bool {
        guard model.wechatRepairAvailable, !model.isPreparingAutomaticWechatRecovery else { return false }
        return switch model.state {
        case .automaticWechatRecoveryDeferred, .automaticWechatRecoveryFailed: true
        default: false
        }
    }

    private func progress(_ text: String) -> some View {
        HStack(spacing: 10) {
            ProgressView().controlSize(.small)
            Text(text)
        }
    }
}
