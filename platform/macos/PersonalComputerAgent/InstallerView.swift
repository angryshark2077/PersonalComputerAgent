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
        case .waitingFullDiskAccess:
            VStack(alignment: .leading, spacing: 8) {
                Label("Full Disk Access is required before the Agent can start.", systemImage: "externaldrive.badge.checkmark")
                Text("In System Settings, add and enable PersonalComputerAgent. Installation continues automatically after access is verified.")
                    .foregroundStyle(.secondary)
            }
        case .waitingLocationAccess:
            progress("Allow Location access once so Wi-Fi SSID and BSSID remain available after restarts.")
        case .waitingApproval:
            progress("Approve Personal Computer Agent in System Settings > General > Login Items.")
        case .starting:
            progress("Starting the local runtime…")
        case .pairing:
            progress("Complete the secure pairing flow in your browser…")
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

    private func progress(_ text: String) -> some View {
        HStack(spacing: 10) {
            ProgressView().controlSize(.small)
            Text(text)
        }
    }
}
