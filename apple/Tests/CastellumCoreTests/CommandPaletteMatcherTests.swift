// Castellum - CommandPaletteMatcher: fuzzy subsequence matching and ranking.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class CommandPaletteMatcherTests: XCTestCase {
    func testEmptyQueryMatchesEverythingWithScoreZero() {
        XCTAssertEqual(CommandPaletteMatcher.score(query: "", target: "Archive"), 0)
    }

    func testExactSubsequenceMatches() {
        XCTAssertNotNil(CommandPaletteMatcher.score(query: "arc", target: "Archive"))
        XCTAssertNotNil(CommandPaletteMatcher.score(query: "ah", target: "Archive"))
    }

    func testOutOfOrderCharactersDoNotMatch() {
        XCTAssertNil(CommandPaletteMatcher.score(query: "hrc", target: "Archive"))
    }

    func testCharactersNotPresentDoNotMatch() {
        XCTAssertNil(CommandPaletteMatcher.score(query: "xyz", target: "Archive"))
    }

    func testMatchIsCaseInsensitive() {
        XCTAssertEqual(
            CommandPaletteMatcher.score(query: "ARCH", target: "Archive"),
            CommandPaletteMatcher.score(query: "arch", target: "Archive")
        )
    }

    func testConsecutiveMatchScoresHigherThanScattered() {
        // "emp" is consecutive in "Empty Trash…"; "e..m..p" scattered elsewhere
        // must score lower for the same query length against the same target
        // length class — compare two DIFFERENT targets where one packs the
        // query into a run and the other spreads it out.
        let consecutive = CommandPaletteMatcher.score(query: "emp", target: "Empty Trash…")!
        let scattered = CommandPaletteMatcher.score(query: "eta", target: "Empty Trash…")!
        XCTAssertGreaterThan(consecutive, scattered)
    }

    func testWordBoundaryMatchScoresHigherThanMidWord() {
        let boundary = CommandPaletteMatcher.score(query: "t", target: "Toggle Read/Unread")!
        let midWord = CommandPaletteMatcher.score(query: "g", target: "Toggle Read/Unread")!
        XCTAssertGreaterThan(boundary, midWord)
    }

    func testRankOrdersByScoreDescendingAndFiltersNonMatches() {
        let specs = [
            CommandRegistry.spec(for: .archive),   // "Archive"
            CommandRegistry.spec(for: .reply),      // "Reply"
            CommandRegistry.spec(for: .replyAll),   // "Reply All"
        ]
        let ranked = CommandPaletteMatcher.rank(query: "repl", commands: specs)
        XCTAssertEqual(Set(ranked.map(\.id)), [.reply, .replyAll], "Archive doesn't match 'repl'")
        XCTAssertTrue(ranked.allSatisfy { $0.id != .archive })
    }

    func testRankKeepsRegistryOrderOnTies() {
        let specs = [CommandRegistry.spec(for: .reply), CommandRegistry.spec(for: .replyAll)]
        // Empty query: both score 0, so registry order (reply, then replyAll) must hold.
        XCTAssertEqual(CommandPaletteMatcher.rank(query: "", commands: specs).map(\.id), [.reply, .replyAll])
    }

    /// #3054: the row shows `help ?? title`, so a word of the SHOWN text must match.
    func testRankMatchesHelpTextShownInRow() {
        let spec = CommandRegistry.spec(for: .layoutClassic)  // shows "Message list above the reading pane"
        XCTAssertEqual(CommandPaletteMatcher.rank(query: "above", commands: [spec]).map(\.id), [.layoutClassic])
        // ...and the menu title still finds it.
        XCTAssertEqual(CommandPaletteMatcher.rank(query: "classic", commands: [spec]).map(\.id), [.layoutClassic])
    }
}
