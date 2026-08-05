import Foundation
import XCTest
@testable import PlatformBridge

final class MessageBodyDecoderTests: XCTestCase {
    func testDecodesLegacyAttributedStringAndRejectsInvalidBodies() {
        let archived = NSArchiver.archivedData(withRootObject: NSAttributedString(string: "Hello from Messages"))
        let decoded = MessageBodyDecoder.decode([
            archived.base64EncodedString(),
            "not-base64",
            Data().base64EncodedString(),
        ])

        XCTAssertEqual(decoded[0], "Hello from Messages")
        XCTAssertNil(decoded[1])
        XCTAssertNil(decoded[2])
    }
}
