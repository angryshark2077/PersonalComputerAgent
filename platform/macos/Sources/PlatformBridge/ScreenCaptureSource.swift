import AppKit
import BridgeProtocol
import CryptoKit
import Darwin
import Foundation

struct ScreenContext: Sendable {
    let locked: Bool
    let appBundleID: String?
    let activityToken: String?

    var payload: [String: JSONValue] {
        [
            "locked": .bool(locked),
            "app_bundle_id": appBundleID.map(JSONValue.string) ?? .null,
            "activity_token": activityToken.map(JSONValue.string) ?? .null,
        ]
    }
}

struct ScreenCaptureResult: Sendable {
    let status: String
    let path: String?
    let appBundleID: String?
    let pixelWidth: Int?
    let pixelHeight: Int?

    var payload: [String: JSONValue] {
        [
            "status": .string(status),
            "path": path.map(JSONValue.string) ?? .null,
            "app_bundle_id": appBundleID.map(JSONValue.string) ?? .null,
            "pixel_width": pixelWidth.map { .number(Double($0)) } ?? .null,
            "pixel_height": pixelHeight.map { .number(Double($0)) } ?? .null,
        ]
    }
}

struct ScreenCaptureSource: Sendable {
    static var productionCaptureDirectory: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library", isDirectory: true)
            .appendingPathComponent("Application Support", isDirectory: true)
            .appendingPathComponent("PersonalComputerAgent", isDirectory: true)
            .appendingPathComponent("ScreenshotSpool", isDirectory: true)
    }

    let captureDirectory: URL

    init(captureDirectory: URL = Self.productionCaptureDirectory) {
        self.captureDirectory = captureDirectory
    }

    func context() -> ScreenContext {
        guard !sessionIsLocked() else {
            return ScreenContext(locked: true, appBundleID: nil, activityToken: nil)
        }
        guard let window = frontmostWindow() else {
            return ScreenContext(locked: false, appBundleID: nil, activityToken: nil)
        }
        return ScreenContext(
            locked: false,
            appBundleID: window.bundleID,
            activityToken: activityToken(window)
        )
    }

    func capture(excludedBundleIDs: Set<String>) -> ScreenCaptureResult {
        guard !sessionIsLocked() else { return skipped("skipped_locked") }
        guard CGPreflightScreenCaptureAccess() else { return skipped("permission_required") }
        guard let window = frontmostWindow() else { return skipped("unavailable") }
        if let bundleID = window.bundleID, excludedBundleIDs.contains(bundleID) {
            return ScreenCaptureResult(
                status: "skipped_excluded",
                path: nil,
                appBundleID: bundleID,
                pixelWidth: nil,
                pixelHeight: nil
            )
        }
        guard let image = CGDisplayCreateImage(window.displayID),
              let jpeg = NSBitmapImageRep(cgImage: image).representation(
                using: .jpeg,
                properties: [.compressionFactor: 0.75]
              ) else {
            return skipped("unavailable", appBundleID: window.bundleID)
        }
        do {
            try prepareCaptureDirectory()
            let fileURL = captureDirectory.appendingPathComponent(
                "\(UUID().uuidString.lowercased()).jpg",
                isDirectory: false
            )
            guard fileURL.deletingLastPathComponent().path == captureDirectory.path else {
                return skipped("unavailable", appBundleID: window.bundleID)
            }
            try jpeg.write(to: fileURL, options: .atomic)
            guard chmod(fileURL.path, 0o600) == 0 else {
                try? FileManager.default.removeItem(at: fileURL)
                return skipped("unavailable", appBundleID: window.bundleID)
            }
            return ScreenCaptureResult(
                status: "captured",
                path: fileURL.path,
                appBundleID: window.bundleID,
                pixelWidth: image.width,
                pixelHeight: image.height
            )
        } catch {
            return skipped("unavailable", appBundleID: window.bundleID)
        }
    }

    private func skipped(_ status: String, appBundleID: String? = nil) -> ScreenCaptureResult {
        ScreenCaptureResult(
            status: status,
            path: nil,
            appBundleID: appBundleID,
            pixelWidth: nil,
            pixelHeight: nil
        )
    }

    private func prepareCaptureDirectory() throws {
        try FileManager.default.createDirectory(
            at: captureDirectory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
        var info = stat()
        guard lstat(captureDirectory.path, &info) == 0,
              info.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR),
              info.st_uid == geteuid(),
              chmod(captureDirectory.path, 0o700) == 0 else {
            throw BridgeServerError.unsafeRunDirectory
        }
    }

    private func sessionIsLocked() -> Bool {
        guard let dictionary = CGSessionCopyCurrentDictionary() as? [String: Any] else { return true }
        if dictionary["CGSSessionScreenIsLocked"] as? Bool == true { return true }
        if dictionary[kCGSessionOnConsoleKey as String] as? Bool == false { return true }
        return dictionary[kCGSessionLoginDoneKey as String] as? Bool != true
    }

    private func frontmostWindow() -> CapturableWindow? {
        guard let windows = CGWindowListCopyWindowInfo(
            [.optionOnScreenOnly, .excludeDesktopElements],
            kCGNullWindowID
        ) as? [[String: Any]] else { return nil }
        for window in windows {
            guard (window[kCGWindowLayer as String] as? Int) == 0,
                  let pidValue = window[kCGWindowOwnerPID as String] as? Int,
                  let number = window[kCGWindowNumber as String] as? Int,
                  let boundsValue = window[kCGWindowBounds as String] as? [String: Any],
                  let bounds = CGRect(dictionaryRepresentation: boundsValue as CFDictionary),
                  bounds.width > 1,
                  bounds.height > 1 else { continue }
            let pid = pid_t(pidValue)
            let bundleID = NSRunningApplication(processIdentifier: pid)?.bundleIdentifier
            return CapturableWindow(
                pid: pid,
                windowNumber: number,
                bundleID: bundleID,
                displayID: displayContaining(bounds)
            )
        }
        return nil
    }

    private func displayContaining(_ windowBounds: CGRect) -> CGDirectDisplayID {
        var displays = [CGDirectDisplayID](repeating: 0, count: 32)
        var count: UInt32 = 0
        guard CGGetActiveDisplayList(UInt32(displays.count), &displays, &count) == .success else {
            return CGMainDisplayID()
        }
        return displays.prefix(Int(count)).max { left, right in
            CGDisplayBounds(left).intersection(windowBounds).area
                < CGDisplayBounds(right).intersection(windowBounds).area
        } ?? CGMainDisplayID()
    }

    private func activityToken(_ window: CapturableWindow) -> String {
        guard let anyInputEvent = CGEventType(rawValue: UInt32.max) else {
            return SHA256.hash(data: Data("\(window.pid):\(window.windowNumber):\(window.displayID)".utf8))
                .map { String(format: "%02x", $0) }
                .joined()
        }
        let secondsSinceInput = CGEventSource.secondsSinceLastEventType(
            .combinedSessionState,
            eventType: anyInputEvent
        )
        let activityBucket = secondsSinceInput <= 5
            ? String(Int(Date().timeIntervalSince1970 / 5))
            : "idle"
        let value = "\(window.pid):\(window.windowNumber):\(window.displayID):\(activityBucket)"
        return SHA256.hash(data: Data(value.utf8)).map { String(format: "%02x", $0) }.joined()
    }
}

private struct CapturableWindow {
    let pid: pid_t
    let windowNumber: Int
    let bundleID: String?
    let displayID: CGDirectDisplayID
}

private extension CGRect {
    var area: CGFloat { isNull ? 0 : width * height }
}
