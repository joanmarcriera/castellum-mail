// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S39 search query language: full-stack (Swift -> FFI -> Rust)
// hostile-input smoke tests, plus the search field's syntax-hint content.
import XCTest
@testable import Castellum

final class SearchQuerySyntaxTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-swift-search-\(UUID().uuidString)")
    }

    /// Every operator, several hostile strings, and a very long input, all
    /// the way through String -> CString -> Rust parser -> SQL -> back. None
    /// of these seed any mail, so every call must return an empty array
    /// without throwing (correctness of matching is proven in
    /// castellum-db's own tests; this test's job is only "does it survive
    /// the FFI round trip").
    func testHostileSearchQueriesNeverThrow() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let queries = [
            "from:alice to:bob cc:carol subject:\"weekly report\"",
            "has:attachment before:2026-09-18 after:2026-01-01",
            "in:inbox is:unread is:flagged is:starred -is:read",
            "-spam \"quoted phrase\" bare -\"another phrase\"",
            "", "   ", "-", "-  -", "::::", "\"unterminated quote",
            "before:garbage after:9999999999999999999",
            "café münchën 日本語 emoji 🎉",
            String(repeating: "a very long query word ", count: 200),
        ]
        for q in queries {
            XCTAssertEqual(try core.search(accountId: "a", query: q, limit: 10).count, 0, "query: \(q)")
            XCTAssertEqual(try core.searchAcrossAccounts(query: q, limit: 10).count, 0, "query: \(q)")
        }
    }

    func testSyntaxHintNamesEveryOperator() {
        let hint = SearchFieldView.syntaxHint
        for op in ["from:", "to:", "cc:", "subject:", "has:attachment", "before:", "after:", "in:", "is:"] {
            XCTAssertTrue(hint.contains(op), "syntax hint is missing \(op)")
        }
    }
}
