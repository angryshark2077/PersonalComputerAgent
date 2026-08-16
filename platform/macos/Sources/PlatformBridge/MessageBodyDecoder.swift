import Foundation

enum MessageBodyDecoder {
    private static let maximumBodyBytes = 4 * 1024 * 1024
    private static let typedstreamHeader = Array("\u{04}\u{0b}streamtyped".utf8)
    private static let attributedStringClasses = [
        Array("NSAttributedString".utf8),
        Array("NSMutableAttributedString".utf8),
    ]
    private static let stringClass = Array("NSString".utf8)

    static func decode(_ encodedBodies: [String]) -> [String?] {
        encodedBodies.map { encoded in
            guard let data = Data(base64Encoded: encoded), data.count <= maximumBodyBytes else {
                return nil
            }
            return decodeLegacyTypedstream(data).flatMap(normalized)
        }
    }

    private static func decodeLegacyTypedstream(_ data: Data) -> String? {
        let bytes = Array(data)
        guard bytes.starts(with: typedstreamHeader),
              bytes.last == 0x86,
              attributedStringClasses.contains(where: { find($0, in: bytes, from: 0) != nil })
        else {
            return nil
        }
        var searchOffset = typedstreamHeader.count
        while let classOffset = find(stringClass, in: bytes, from: searchOffset) {
            let metadataStart = classOffset + stringClass.count
            let metadataEnd = min(bytes.count, metadataStart + 16)
            if let marker = bytes[metadataStart..<metadataEnd].firstIndex(of: 0x2b),
               let (length, contentStart) = decodedLength(bytes, at: marker + 1),
               length <= maximumBodyBytes,
               contentStart <= bytes.count,
               length <= bytes.count - contentStart
            {
                let content = bytes[contentStart..<(contentStart + length)]
                if let value = String(bytes: content, encoding: .utf8) {
                    return value
                }
            }
            searchOffset = metadataStart
        }
        return nil
    }

    private static func decodedLength(_ bytes: [UInt8], at offset: Int) -> (Int, Int)? {
        guard offset < bytes.count else { return nil }
        switch bytes[offset] {
        case 0...0x7f:
            return (Int(bytes[offset]), offset + 1)
        case 0x81:
            return fixedWidthLength(bytes, at: offset + 1, byteCount: 2)
        case 0x82:
            return fixedWidthLength(bytes, at: offset + 1, byteCount: 4)
        case 0x83:
            return fixedWidthLength(bytes, at: offset + 1, byteCount: 8)
        default:
            return nil
        }
    }

    private static func fixedWidthLength(
        _ bytes: [UInt8],
        at offset: Int,
        byteCount: Int
    ) -> (Int, Int)? {
        guard offset <= bytes.count, byteCount <= bytes.count - offset else { return nil }
        var value: UInt64 = 0
        for index in 0..<byteCount {
            value |= UInt64(bytes[offset + index]) << UInt64(index * 8)
        }
        guard value <= UInt64(Int.max) else { return nil }
        return (Int(value), offset + byteCount)
    }

    private static func find(_ needle: [UInt8], in bytes: [UInt8], from offset: Int) -> Int? {
        guard !needle.isEmpty, offset <= bytes.count, needle.count <= bytes.count - offset else {
            return nil
        }
        for candidate in offset...(bytes.count - needle.count)
        where bytes[candidate..<(candidate + needle.count)].elementsEqual(needle) {
            return candidate
        }
        return nil
    }

    private static func normalized(_ value: String) -> String? {
        let value = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return value.isEmpty || value.count > 1_000_000 ? nil : value
    }
}
