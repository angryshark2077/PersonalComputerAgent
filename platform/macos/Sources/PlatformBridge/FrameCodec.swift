import Foundation

enum FrameCodecError: Error, Equatable, Sendable {
    case zeroLength
    case oversized
    case invalidUTF8
    case truncated
    case disconnected
    case inputOutput
}

enum FrameCodec {
    static let maximumFrameBytes = 1024 * 1024

    static func encode(_ payload: Data) throws -> Data {
        guard !payload.isEmpty else { throw FrameCodecError.zeroLength }
        guard payload.count <= maximumFrameBytes else { throw FrameCodecError.oversized }
        guard String(data: payload, encoding: .utf8) != nil else { throw FrameCodecError.invalidUTF8 }

        var length = UInt32(payload.count).bigEndian
        var frame = withUnsafeBytes(of: &length) { Data($0) }
        frame.append(payload)
        return frame
    }
}

struct FrameDecoder: Sendable {
    private var buffer = Data()
    private var expectedLength: Int?

    var bufferedByteCount: Int { buffer.count }

    mutating func append<Bytes: DataProtocol>(_ bytes: Bytes) throws -> [Data] {
        buffer.append(contentsOf: bytes)
        var frames: [Data] = []

        while true {
            if expectedLength == nil {
                guard buffer.count >= 4 else { break }
                let length = buffer.prefix(4).reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
                guard length != 0 else {
                    reset()
                    throw FrameCodecError.zeroLength
                }
                guard length <= UInt32(FrameCodec.maximumFrameBytes) else {
                    reset()
                    throw FrameCodecError.oversized
                }
                buffer.removeFirst(4)
                expectedLength = Int(length)
            }

            guard let expectedLength, buffer.count >= expectedLength else { break }
            let payload = Data(buffer.prefix(expectedLength))
            guard String(data: payload, encoding: .utf8) != nil else {
                reset()
                throw FrameCodecError.invalidUTF8
            }
            buffer.removeFirst(expectedLength)
            self.expectedLength = nil
            frames.append(payload)
        }

        return frames
    }

    mutating func finish() throws {
        guard buffer.isEmpty, expectedLength == nil else { throw FrameCodecError.truncated }
    }

    private mutating func reset() {
        buffer.removeAll(keepingCapacity: false)
        expectedLength = nil
    }
}
