import Foundation
import XCTest
@testable import PlatformBridge

final class MessageBodyDecoderTests: XCTestCase {
    func testDecodesLegacyAttributedStringsAndRejectsInvalidBodies() {
        let ascii = NSArchiver.archivedData(withRootObject: NSAttributedString(string: "Hello from Messages"))
        let unicode = NSArchiver.archivedData(
            withRootObject: NSMutableAttributedString(string: "来自 Messages 的中文正文")
        )
        let decoded = MessageBodyDecoder.decode([
            ascii.base64EncodedString(),
            unicode.base64EncodedString(),
            "not-base64",
            Data().base64EncodedString(),
        ])

        XCTAssertEqual(decoded[0], "Hello from Messages")
        XCTAssertEqual(decoded[1], "来自 Messages 的中文正文")
        XCTAssertNil(decoded[2])
        XCTAssertNil(decoded[3])
    }

    func testRejectsTruncatedAndNonAttributedTypedstreams() {
        let attributed = NSArchiver.archivedData(
            withRootObject: NSAttributedString(string: String(repeating: "长", count: 200))
        )
        let unrelated = NSArchiver.archivedData(withRootObject: NSArray(object: "not a message body"))
        let truncated = attributed.prefix(attributed.count / 2)

        let decoded = MessageBodyDecoder.decode([
            truncated.base64EncodedString(),
            unrelated.base64EncodedString(),
        ])

        XCTAssertNil(decoded[0])
        XCTAssertNil(decoded[1])
    }
}
