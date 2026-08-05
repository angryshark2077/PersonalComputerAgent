import Foundation
import Testing
@testable import PlatformBridge

@Suite("Photo library source")
struct PhotoLibrarySourceTests {
    @Test("PhotoKit fetch only uses supported creation-date ordering")
    func fetchOptionsUseSupportedOrdering() throws {
        let cutoff = Date(timeIntervalSince1970: 1_700_000_000)
        let options = PhotoLibrarySource.fetchOptions(cutoff: cutoff)

        let descriptor = try #require(options.sortDescriptors?.only)
        #expect(descriptor.key == "creationDate")
        #expect(descriptor.ascending)
        #expect(options.predicate?.evaluate(with: ["creationDate": cutoff]) == true)
        #expect(options.predicate?.evaluate(with: ["creationDate": cutoff.addingTimeInterval(-1)]) == false)
    }
}

private extension Array {
    var only: Element? {
        count == 1 ? self[0] : nil
    }
}
