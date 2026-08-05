import Foundation

enum MessageBodyDecoder {
    static func decode(_ encodedBodies: [String]) -> [String?] {
        encodedBodies.map { encoded in
            guard let data = Data(base64Encoded: encoded), data.count <= 4 * 1024 * 1024 else {
                return nil
            }
            if let attributed = NSUnarchiver.unarchiveObject(with: data) as? NSAttributedString {
                return normalized(attributed.string)
            }
            return nil
        }
    }

    private static func normalized(_ value: String) -> String? {
        let value = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return value.isEmpty || value.count > 1_000_000 ? nil : value
    }
}
