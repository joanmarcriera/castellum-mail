// Castellum - plain-text structure: blocks, fold thresholds, signature stripping and the
// conservative re-wrap (MM-17, slices 3 and 4).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class PlainTextStructureTests: XCTestCase {
    private typealias S = PlainTextStructure

    // MARK: parse

    func testNestedQuotesSplitByDepth() {
        let blocks = S.parse("intro\n> a\n> > b\n> > c\nreply")
        XCTAssertEqual(blocks, [
            .text(lines: ["intro"]), .quote(depth: 1, lines: ["> a"]),
            .quote(depth: 2, lines: ["> > b", "> > c"]), .text(lines: ["reply"]),
        ])
    }

    func testMarkersWithoutSpacesAndWithSpacesCountTheSameDepth() {
        XCTAssertEqual(S.quoteDepth(of: ">>> x").depth, 3)
        XCTAssertEqual(S.quoteDepth(of: "> > > x").depth, 3)
        XCTAssertEqual(S.quoteDepth(of: ">x").depth, 1)
        XCTAssertEqual(S.quoteDepth(of: "not quoted > x").depth, 0)
        let (_, start) = S.quoteDepth(of: "> > hello")
        XCTAssertEqual(String("> > hello"[start...]), "hello")
    }

    func testInterleavedRepliesKeepTheirOrder() {
        let body = "Q1\n> one\n> two\nA1\n> three\nA2"
        XCTAssertEqual(S.parse(body).map(\.lines), [["Q1"], ["> one", "> two"], ["A1"], ["> three"], ["A2"]])
    }

    func testAStrictSignatureStartsAtTheDelimiterAndRunsToTheEnd() {
        let body = "Hello\n\n-- \nAda\nLondon"
        let blocks = S.parse(body)
        XCTAssertEqual(blocks, [.text(lines: ["Hello", ""]), .signature(lines: ["-- ", "Ada", "London"])])
    }

    func testABareDelimiterCountsForFoldingNearTheEnd() {
        let blocks = S.parse("Hello\n--\nMarc")
        XCTAssertEqual(blocks.last, .signature(lines: ["--", "Marc"]))
    }

    func testABareDelimiterFarFromTheEndIsJustText() {
        let tail = (1...20).map { "line \($0)" }.joined(separator: "\n")
        let blocks = S.parse("Hello\n--\n" + tail)
        XCTAssertFalse(blocks.contains { if case .signature = $0 { return true } else { return false } })
    }

    func testADelimiterInsideAQuoteIsNotASignature() {
        let blocks = S.parse("reply\n> text\n> -- \n> Sender sig\nmore")
        XCTAssertFalse(blocks.contains { if case .signature = $0 { return true } else { return false } })
    }

    func testASignatureAboveACastellumStyleQuoteDoesNotSwallowTheQuote() {
        let body = "Thanks.\n\n-- \nMarc\n\nOn Mon, Ada wrote:\n> original\n> text"
        let blocks = S.parse(body)
        XCTAssertEqual(blocks.map(\.lines), [["Thanks.", ""], ["-- ", "Marc"], ["", "On Mon, Ada wrote:"], ["> original", "> text"]])
        guard case .signature(let sig) = blocks[1] else { return XCTFail("expected a signature block") }
        XCTAssertEqual(sig, ["-- ", "Marc"])
    }

    func testEmptyAndWhitespaceOnlyBodies() {
        XCTAssertEqual(S.parse(""), [])
        XCTAssertEqual(S.parse("   \n  "), [.text(lines: ["   ", "  "])])
    }

    func testCRLFBodiesParseLikeLF() {
        XCTAssertEqual(S.parse("a\r\n> b\r\nc"), S.parse("a\n> b\nc"))
    }

    // MARK: folding

    func testQuotesFoldFromEightLinesAndSignaturesFromFive() {
        let q7 = S.Block.quote(depth: 1, lines: Array(repeating: "> x", count: 7))
        let q8 = S.Block.quote(depth: 1, lines: Array(repeating: "> x", count: 8))
        XCTAssertFalse(S.startsFolded(q7))
        XCTAssertTrue(S.startsFolded(q8))
        XCTAssertFalse(S.startsFolded(.signature(lines: ["-- ", "a", "b", "c"])))
        XCTAssertTrue(S.startsFolded(.signature(lines: ["-- ", "a", "b", "c", "d"])))
        XCTAssertFalse(S.startsFolded(.text(lines: Array(repeating: "x", count: 50))), "prose never folds")
    }

    func testFoldLabels() {
        XCTAssertEqual(S.foldLabel(.quote(depth: 1, lines: Array(repeating: "> x", count: 14))), "14 quoted lines")
        XCTAssertEqual(S.foldLabel(.signature(lines: ["-- "])), "Signature")
    }

    func testAHugeBodyParsesInBoundedTime() {
        let body = (0..<20_000).map { $0 % 7 == 0 ? "> quoted \($0)" : "line \($0)" }.joined(separator: "\n")
        let start = Date()
        let blocks = S.parse(body)
        XCTAssertLessThan(Date().timeIntervalSince(start), 2.0)
        XCTAssertFalse(blocks.isEmpty)
    }

    // MARK: strippingSignature

    func testStripsAStrictSignatureAndKeepsTheRest() {
        XCTAssertEqual(S.strippingSignature("Hello\n\n-- \nAda\nLondon"), "Hello")
    }

    func testAKeepsABareDelimiterSignatureBecauseStrippingIsDestructive() {
        let body = "Hello\n--\nMarc"
        XCTAssertEqual(S.strippingSignature(body), body)
    }

    func testStrippingKeepsQuotedTextThatFollowsTheSignature() {
        let body = "Thanks.\n\n-- \nMarc\n\nOn Mon, Ada wrote:\n> original"
        XCTAssertEqual(S.strippingSignature(body), "Thanks.\n\nOn Mon, Ada wrote:\n> original")
    }

    func testNoSignatureMeansUnchanged() {
        XCTAssertEqual(S.strippingSignature("plain\nbody"), "plain\nbody")
        XCTAssertEqual(S.strippingSignature(""), "")
    }

    func testADelimiterInsideAQuoteIsNotStripped() {
        let body = "reply\n> a\n> -- \n> sig"
        XCTAssertEqual(S.strippingSignature(body), body)
    }

    // MARK: rewrapped

    private let l72a = "This is a hard wrapped paragraph that a mail client broke at roughly"   // 69
    private let l72b = "seventy columns, so the quote below it looks like a comb when it is"   // 68
    private let tail = "quoted with a prefix."

    func testAHardWrappedParagraphJoins() {
        XCTAssertGreaterThanOrEqual(l72a.count, 60)
        XCTAssertEqual(S.rewrapped("\(l72a)\n\(l72b)\n\(tail)"), "\(l72a) \(l72b) \(tail)")
    }

    func testQuotedParagraphsJoinPerDepthAndKeepTheirMarkers() {
        let body = "> \(l72a)\n> \(l72b)\n> \(tail)"
        XCTAssertEqual(S.rewrapped(body), "> \(l72a) \(l72b) \(tail)")
    }

    func testNeverJoinsAcrossQuoteDepths() {
        // The unquoted line stays apart from the quoted ones; the quoted ones still join each other.
        let body = "\(l72a)\n> \(l72b)\n> \(tail)"
        XCTAssertEqual(S.rewrapped(body), "\(l72a)\n> \(l72b) \(tail)")
        let down = "> \(l72a)\n\(l72b)"
        XCTAssertEqual(S.rewrapped(down), down)
        let nested = "> \(l72a)\n> > \(l72b)"
        XCTAssertEqual(S.rewrapped(nested), nested)
    }

    func testShortLinesAreLeftAlone() {
        let body = "Dear Ada,\n\nThanks for the note.\nSee you soon."
        XCTAssertEqual(S.rewrapped(body), body)
    }

    func testCodeTablesAndListsAreLeftAlone() {
        let code = "\(l72a)\n    indented code continues here"
        XCTAssertEqual(S.rewrapped(code), code)
        let table = "\(l72a)\n| a | b |"
        XCTAssertEqual(S.rewrapped(table), table)
        for item in ["- item", "* item", "+ item", "• item", "1. item", "12) item", "a) item"] {
            let list = "\(l72a)\n\(item)"
            XCTAssertEqual(S.rewrapped(list), list, item)
        }
        let spaced = "\(l72a)\ncolumns   aligned   here"
        XCTAssertEqual(S.rewrapped(spaced), spaced, "3+ spaces mean alignment")
        let tabbed = "\(l72a)\nname\tvalue"
        XCTAssertEqual(S.rewrapped(tabbed), tabbed)
    }

    func testAHardBreakWithTwoTrailingSpacesIsKept() {
        let body = "\(l72a)  \n\(l72b)"
        XCTAssertEqual(S.rewrapped(body), body)
    }

    func testBlankLinesSeparateParagraphs() {
        let body = "\(l72a)\n\n\(l72b)"
        XCTAssertEqual(S.rewrapped(body), body)
    }

    func testAnAlreadyUnflowedParagraphPassesThrough() {
        let long = String(repeating: "word ", count: 60)
        XCTAssertEqual(S.rewrapped(long), long)
    }

    func testCatalanAndSpanishProseJoin() {
        let ca1 = "Hola Marc, et escric per confirmar la reunió de dimarts que vam acordar"
        let ca2 = "la setmana passada amb l’equip de la universitat."
        XCTAssertEqual(S.rewrapped("\(ca1)\n\(ca2)"), "\(ca1) \(ca2)")
    }
}
