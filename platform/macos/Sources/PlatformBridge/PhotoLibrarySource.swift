import AppKit
import Foundation
import Photos
import UniformTypeIdentifiers
import BridgeProtocol

struct PhotoLibrarySource: Sendable {
    static let maximumBatchSize = 50

    func authorizationPayload() -> [String: JSONValue] {
        ["status": .string(Self.authorizationStatus())]
    }

    func list(afterDate: Date?, afterIdentifier: String?, cutoff: Date, limit: Int) -> [String: JSONValue] {
        guard PHPhotoLibrary.authorizationStatus(for: .readWrite) == .authorized else {
            return ["status": .string("permission_required"), "assets": .array([])]
        }
        let options = Self.fetchOptions(cutoff: cutoff)
        let result = PHAsset.fetchAssets(with: options)
        var assets: [JSONValue] = []
        result.enumerateObjects { asset, _, stop in
            guard assets.count < min(max(limit, 1), Self.maximumBatchSize) else {
                stop.pointee = true
                return
            }
            guard let creationDate = asset.creationDate,
                  isAfterCursor(creationDate, asset.localIdentifier, afterDate, afterIdentifier),
                  let resource = Self.primaryResource(for: asset) else { return }
            let albums = PHAssetCollection.fetchAssetCollectionsContaining(
                asset,
                with: .album,
                options: nil
            )
            var albumNames: [JSONValue] = []
            albums.enumerateObjects { collection, _, _ in
                if let title = collection.localizedTitle?.trimmingCharacters(in: .whitespacesAndNewlines),
                   !title.isEmpty,
                   !albumNames.contains(.string(title)) {
                    albumNames.append(.string(title))
                }
            }
            let mediaType = asset.mediaType == .video ? "video" : "image"
            let mimeType = UTType(resource.uniformTypeIdentifier)?.preferredMIMEType
                ?? (mediaType == "video" ? "video/quicktime" : "image/jpeg")
            assets.append(.object([
                "local_identifier": .string(asset.localIdentifier),
                "created_at": .string(Self.format(creationDate)),
                "media_type": .string(mediaType),
                "original_filename": .string(resource.originalFilename),
                "mime_type": .string(mimeType),
                "pixel_width": .number(Double(asset.pixelWidth)),
                "pixel_height": .number(Double(asset.pixelHeight)),
                "duration_seconds": .number(asset.duration),
                "album_names": .array(albumNames),
            ]))
        }
        return ["status": .string("available"), "assets": .array(assets)]
    }

    func export(localIdentifier: String, fileName: String) -> [String: JSONValue] {
        guard PHPhotoLibrary.authorizationStatus(for: .readWrite) == .authorized else {
            return ["status": .string("permission_required")]
        }
        guard UUID(uuidString: fileName) != nil else { return ["status": .string("unavailable")] }
        let assets = PHAsset.fetchAssets(withLocalIdentifiers: [localIdentifier], options: nil)
        guard let asset = assets.firstObject, let resource = Self.primaryResource(for: asset) else {
            return ["status": .string("unavailable")]
        }
        let root = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/PersonalComputerAgent/PhotoSpool", isDirectory: true)
        do {
            try FileManager.default.createDirectory(
                at: root,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            let destination = root.appendingPathComponent(fileName, isDirectory: false)
            guard destination.deletingLastPathComponent().standardizedFileURL == root.standardizedFileURL else {
                return ["status": .string("unavailable")]
            }
            if FileManager.default.fileExists(atPath: destination.path) {
                let values = try destination.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey])
                guard values.isRegularFile == true, values.isSymbolicLink != true else {
                    return ["status": .string("unavailable")]
                }
                return ["status": .string("exported"), "path": .string(destination.path)]
            }
            let semaphore = DispatchSemaphore(value: 0)
            final class ResultBox: @unchecked Sendable { var error: Error? }
            let result = ResultBox()
            let requestOptions = PHAssetResourceRequestOptions()
            requestOptions.isNetworkAccessAllowed = true
            PHAssetResourceManager.default().writeData(
                for: resource,
                toFile: destination,
                options: requestOptions
            ) { error in
                result.error = error
                semaphore.signal()
            }
            guard semaphore.wait(timeout: .now() + 25) == .success, result.error == nil else {
                try? FileManager.default.removeItem(at: destination)
                return ["status": .string("unavailable")]
            }
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
            return ["status": .string("exported"), "path": .string(destination.path)]
        } catch {
            return ["status": .string("unavailable")]
        }
    }

    @MainActor
    static func requestAuthorization() async -> Bool {
        let currentStatus = PHPhotoLibrary.authorizationStatus(for: .readWrite)
        FileHandle.standardError.write(Data("PCAPlatformBridge: Photos authorization status before request: \(currentStatus.rawValue)\n".utf8))
        if currentStatus == .authorized { return true }
        if currentStatus != .notDetermined { return false }

        let activationWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1, height: 1),
            styleMask: .borderless,
            backing: .buffered,
            defer: false
        )
        activationWindow.alphaValue = 0
        activationWindow.makeKeyAndOrderFront(nil)
        defer { activationWindow.close() }
        NSApplication.shared.activate(ignoringOtherApps: true)
        let clock = ContinuousClock()
        let activationDeadline = clock.now.advanced(by: .seconds(2))
        while !NSApplication.shared.isActive, clock.now < activationDeadline {
            try? await Task.sleep(for: .milliseconds(100))
        }
        let status = await PHPhotoLibrary.requestAuthorization(for: .readWrite)
        FileHandle.standardError.write(Data("PCAPlatformBridge: Photos authorization status after request: \(status.rawValue)\n".utf8))
        return status == .authorized
    }

    private static func authorizationStatus() -> String {
        switch PHPhotoLibrary.authorizationStatus(for: .readWrite) {
        case .authorized: "available"
        case .notDetermined: "not_determined"
        case .denied, .restricted, .limited: "permission_required"
        @unknown default: "unavailable"
        }
    }

    private static func format(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.string(from: date)
    }

    static func fetchOptions(cutoff: Date) -> PHFetchOptions {
        let options = PHFetchOptions()
        options.predicate = NSPredicate(format: "creationDate >= %@", cutoff as NSDate)
        options.sortDescriptors = [NSSortDescriptor(key: "creationDate", ascending: true)]
        return options
    }

    private static func primaryResource(for asset: PHAsset) -> PHAssetResource? {
        let resources = PHAssetResource.assetResources(for: asset)
        if asset.mediaType == .video {
            return resources.first(where: { $0.type == .fullSizeVideo })
                ?? resources.first(where: { $0.type == .video })
        }
        return resources.first(where: { $0.type == .fullSizePhoto })
            ?? resources.first(where: { $0.type == .photo })
    }

    private func isAfterCursor(
        _ date: Date,
        _ identifier: String,
        _ afterDate: Date?,
        _ afterIdentifier: String?
    ) -> Bool {
        guard let afterDate else { return true }
        if date > afterDate { return true }
        return date == afterDate && identifier > (afterIdentifier ?? "")
    }
    static func parseDate(_ value: String) -> Date? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.date(from: value)
    }
}
