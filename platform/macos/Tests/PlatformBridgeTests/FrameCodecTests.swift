import Foundation
@testable import PlatformBridge
import XCTest

final class FrameCodecTests: XCTestCase {
    func testFragmentedPrefixAndBodyProduceOneExactFrame() throws {
        let payload = Data("{\"ok\":true}".utf8)
        let encoded = try FrameCodec.encode(payload)
        var decoder = FrameDecoder()

        XCTAssertEqual(try decoder.append(encoded.prefix(2)), [])
        XCTAssertEqual(try decoder.append(encoded.dropFirst(2).prefix(5)), [])
        XCTAssertEqual(try decoder.append(encoded.dropFirst(7)), [payload])
    }

    func testBackToBackFramesAreNotCoalesced() throws {
        let first = Data("{\"n\":1}".utf8)
        let second = Data("{\"n\":2}".utf8)
        var wire = try FrameCodec.encode(first)
        wire.append(try FrameCodec.encode(second))
        var decoder = FrameDecoder()

        XCTAssertEqual(try decoder.append(wire), [first, second])
    }

    func testOversizedLengthIsRejectedFromPrefixAlone() throws {
        var decoder = FrameDecoder()
        let oversized = UInt32(FrameCodec.maximumFrameBytes + 1).bigEndian
        let prefix = withUnsafeBytes(of: oversized) { Data($0) }

        XCTAssertThrowsError(try decoder.append(prefix)) { error in
            XCTAssertEqual(error as? FrameCodecError, .oversized)
        }
        XCTAssertEqual(decoder.bufferedByteCount, 0)
    }

    func testZeroLengthAndInvalidUTF8AreRejected() throws {
        var zeroDecoder = FrameDecoder()
        XCTAssertThrowsError(try zeroDecoder.append(Data(repeating: 0, count: 4))) { error in
            XCTAssertEqual(error as? FrameCodecError, .zeroLength)
        }

        var utf8Decoder = FrameDecoder()
        let invalid = Data([0, 0, 0, 1, 0xff])
        XCTAssertThrowsError(try utf8Decoder.append(invalid)) { error in
            XCTAssertEqual(error as? FrameCodecError, .invalidUTF8)
        }
    }
}
