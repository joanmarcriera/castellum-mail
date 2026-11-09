// Castellum - the template library (MM-7, #2697).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class TemplateLibraryTests: XCTestCase {
    func testSetClearAndPerAccountScoping() {
        var lib = TemplateLibrary()
        lib.setAttribution("${sender} wrote:", for: "a")
        XCTAssertEqual(lib.attribution(for: "a"), "${sender} wrote:")
        XCTAssertNil(lib.attribution(for: "b"))
        lib.setAttribution("   \n", for: "a")
        XCTAssertNil(lib.attribution(for: "a"), "a blank format removes the entry")
    }

    func testCodableRoundTripAndForwardCompatibleDecode() throws {
        var lib = TemplateLibrary()
        lib.setAttribution("x", for: "a")
        let data = try JSONEncoder().encode(lib)
        XCTAssertEqual(try JSONDecoder().decode(TemplateLibrary.self, from: data), lib)
        // A blob with no keys (an older build) and one with future keys both load.
        XCTAssertEqual(try JSONDecoder().decode(TemplateLibrary.self, from: Data("{}".utf8)), TemplateLibrary())
        let future = Data(#"{"replyAttribution":{"a":"y"},"snippets":[],"inlineTriggerEnabled":true}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(TemplateLibrary.self, from: future).attribution(for: "a"), "y")
    }

    func testPruneKeepsOnlyListedAccounts() {
        var lib = TemplateLibrary()
        lib.setAttribution("1", for: "a")
        lib.setAttribution("2", for: "b")
        lib.prune(keepingAccounts: ["b"])
        XCTAssertNil(lib.attribution(for: "a"))
        XCTAssertEqual(lib.attribution(for: "b"), "2")
    }

    @MainActor
    func testPreferencesPersistAndBackUpAnUndecodableBlob() {
        let suite = "castellum-template-lib-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let first = Preferences(defaults: defaults)
        first.templateLibrary.setAttribution("${sender} wrote:", for: "a")
        XCTAssertEqual(Preferences(defaults: defaults).templateLibrary.attribution(for: "a"), "${sender} wrote:")

        let junk = Data("not json".utf8)
        defaults.set(junk, forKey: "templateLibrary")
        let recovered = Preferences(defaults: defaults)
        XCTAssertEqual(recovered.templateLibrary, TemplateLibrary())
        XCTAssertEqual(defaults.data(forKey: "templateLibrary.undecodable-backup"), junk)
    }
}
