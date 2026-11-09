// Castellum - the plain-text viewer's rendering, fold state and selection plumbing
// (MM-17, slice 3). Pointer behaviour (click a fold line, drag-select) is Marc's GUI check.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class PlainFoldStateTests: XCTestCase {
    private let blocks: [PlainTextStructure.Block] = [
        .text(lines: ["hi"]),
        .quote(depth: 1, lines: Array(repeating: "> q", count: 12)),   // starts folded
        .quote(depth: 1, lines: ["> short"]),                            // starts open
        .signature(lines: ["-- ", "a", "b", "c", "d", "e"]),             // starts folded
    ]

    func testDefaultsFollowTheStartsFoldedRule() {
        let f = PlainFoldState()
        XCTAssertEqual(f.foldedCount(blocks), 2)
        XCTAssertFalse(f.isFolded(index: 0, block: blocks[0]))
        XCTAssertTrue(f.isFolded(index: 1, block: blocks[1]))
        XCTAssertFalse(f.isFolded(index: 2, block: blocks[2]))
    }

    func testToggleFlipsOneBlockBothWays() {
        var f = PlainFoldState()
        f.toggle(index: 1)
        XCTAssertEqual(f.foldedCount(blocks), 1)
        f.toggle(index: 1)
        XCTAssertEqual(f.foldedCount(blocks), 2)
        f.toggle(index: 2)   // a short quote can be closed too
        XCTAssertTrue(f.isFolded(index: 2, block: blocks[2]))
    }

    func testExpandAllOpensEveryFoldedBlockAndResetRestoresTheDefaults() {
        var f = PlainFoldState()
        f.expandAll(blocks)
        XCTAssertEqual(f.foldedCount(blocks), 0)
        f.reset()
        XCTAssertEqual(f.foldedCount(blocks), 2)
    }
}

final class PlainTextRenderingTests: XCTestCase {
    private func body() -> String {
        let quote = (1...12).map { "> quoted \($0)" }.joined(separator: "\n")
        return "Thanks.\n\nOn Mon, Ada wrote:\n\(quote)\n\nSee you.\n-- \nMarc\nl2\nl3\nl4\nl5"
    }

    private func render(_ fold: PlainFoldState = PlainFoldState(), folding: Bool = true) -> NSAttributedString {
        PlainTextRendering.attributedString(blocks: PlainTextStructure.parse(body()), fold: fold, foldingEnabled: folding)
    }

    func testFoldedBlocksBecomeOneChromeLineEach() {
        let s = render().string
        XCTAssertTrue(s.contains("▸ 12 quoted lines"), s)
        XCTAssertTrue(s.contains("▸ Signature"), s)
        XCTAssertFalse(s.contains("quoted 5"), "folded text is not in the view")
        XCTAssertTrue(s.contains("Thanks.") && s.contains("See you."))
    }

    func testAnExpandedBlockShowsItsLinesAndAWayBack() {
        var fold = PlainFoldState()
        fold.toggle(index: 1)   // the quote
        let s = render(fold).string
        XCTAssertTrue(s.contains("quoted 5"))
        XCTAssertTrue(s.contains("▾ 12 quoted lines"), "a way to close it again")
        XCTAssertTrue(s.contains("▸ Signature"), "the other block stays folded")
    }

    func testFoldingOffRendersTheWholeBodyWithoutChrome() {
        let s = render(folding: false)
        XCTAssertFalse(s.string.contains("▸") || s.string.contains("▾"))
        XCTAssertTrue(s.string.contains("quoted 12") && s.string.contains("l5"))
    }

    func testFoldLinesCarryALinkAndTheChromeMark() {
        let s = render()
        let range = (s.string as NSString).range(of: "▸ 12 quoted lines")
        let attrs = s.attributes(at: range.location, effectiveRange: nil)
        XCTAssertNotNil(attrs[.castellumChrome])
        XCTAssertEqual(PlainTextRendering.foldIndex(from: attrs[.link] as Any), 1)
    }

    func testFoldIndexParsesOnlyOurScheme() {
        XCTAssertEqual(PlainTextRendering.foldIndex(from: URL(string: "castellum-fold:3")!), 3)
        XCTAssertEqual(PlainTextRendering.foldIndex(from: "castellum-fold:0"), 0)
        XCTAssertNil(PlainTextRendering.foldIndex(from: URL(string: "https://example.org/")!))
        XCTAssertNil(PlainTextRendering.foldIndex(from: URL(string: "castellum-fold:x")!))
    }

    func testQuotedTextIsTintedAndIndentedByDepth() {
        var fold = PlainFoldState()
        fold.toggle(index: 1)
        let s = render(fold)
        let range = (s.string as NSString).range(of: "> quoted 3")
        let attrs = s.attributes(at: range.location, effectiveRange: nil)
        XCTAssertNotNil(attrs[.backgroundColor], "a quote is tinted")
        XCTAssertGreaterThan((attrs[.paragraphStyle] as? NSParagraphStyle)?.headIndent ?? 0, 0)
        let plain = s.attributes(at: (s.string as NSString).range(of: "Thanks.").location, effectiveRange: nil)
        XCTAssertNil(plain[.backgroundColor], "prose is not")
    }

    func testTheSelectionNeverContainsTheFoldLines() {
        let s = render()
        let whole = NSRange(location: 0, length: s.length)
        let selected = PlainTextRendering.selectedText(in: s, range: whole)
        XCTAssertTrue(selected.contains("Thanks."))
        XCTAssertFalse(selected.contains("▸"), selected)
        XCTAssertEqual(PlainTextRendering.selectedText(in: s, range: NSRange(location: 0, length: 0)), "")
    }
}

@MainActor
final class PlainTextBodyViewModelTests: XCTestCase {
    private func makeModel() -> AppModel {
        AppModel(profileDirectory: TestScratch.root.appendingPathComponent("castellum-fold-\(UUID().uuidString)"))
    }

    private func detail(id: String, body: String) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: id, accountId: "a", mailboxId: "a:INBOX", subject: "s", fromName: nil, fromAddress: "x@y.test",
                sentAt: nil, snippet: nil, hasAttachments: false, hasBody: true, isRead: true, isStarred: false,
                threadCount: 1),
            rfc822MessageId: nil, addresses: [], textBody: body, sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
    }

    func testFoldStateAndSelectionResetWhenAnotherMessageOpens() {
        let model = makeModel()
        let quote = (1...12).map { "> q\($0)" }.joined(separator: "\n")
        model.detail = detail(id: "m1", body: "hi\n\(quote)")
        model.plainFold.toggle(index: 1)
        model.readingSelection = ReadingSelection(messageId: "m1", text: "hi")
        XCTAssertEqual(model.plainFold.foldedCount(PlainTextStructure.parse(model.detail!.textBody!)), 0)
        model.detail = detail(id: "m1", body: "hi\n\(quote)")   // same message re-published (a body refresh)
        XCTAssertEqual(model.plainFold.toggled, [1], "the same message keeps its folds")
        XCTAssertNotNil(model.readingSelection)
        model.detail = detail(id: "m2", body: "other")
        XCTAssertTrue(model.plainFold.toggled.isEmpty, "another message starts from the default folds")
        XCTAssertNil(model.readingSelection, "a selection never follows to another message")
    }

    func testExpandAllQuotesOpensEverything() {
        let model = makeModel()
        let quote = (1...12).map { "> q\($0)" }.joined(separator: "\n")
        model.detail = detail(id: "m1", body: "hi\n\(quote)\n-- \na\nb\nc\nd")
        let blocks = PlainTextStructure.parse(model.detail!.textBody!)
        XCTAssertEqual(model.plainFold.foldedCount(blocks), 2)
        model.expandAllQuotes()
        XCTAssertEqual(model.plainFold.foldedCount(blocks), 0)
    }

    func testFoldingPreferencesDefaultOnAndPersist() {
        let suite = "castellum-fold-prefs-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let prefs = Preferences(defaults: defaults)
        XCTAssertTrue(prefs.foldQuotes)
        XCTAssertTrue(prefs.quoteRewrapFixed)
        prefs.foldQuotes = false
        XCTAssertFalse(Preferences(defaults: defaults).foldQuotes)
    }

    func testTheViewCommandIsRegisteredAndNeedsALoadedBody() {
        let spec = CommandRegistry.spec(for: .expandAllQuotes)
        XCTAssertEqual(spec.menu?.name, .view)
        XCTAssertEqual(spec.notification, .castellumExpandAllQuotes)
        var ctx = PaletteContext(hasSelectedMessage: true, messageBodyAvailable: false, aiEnabled: false)
        XCTAssertFalse(CommandPaletteAvailability.isEnabled(.expandAllQuotes, context: ctx))
        ctx.messageBodyAvailable = true
        XCTAssertTrue(CommandPaletteAvailability.isEnabled(.expandAllQuotes, context: ctx))
    }
}
