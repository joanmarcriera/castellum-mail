// Castellum - inline `;abbrev` + Tab (MM-7, slice 3): the matcher, the
// text-view replacement, and the switch that keeps it all OFF by default.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class SnippetTriggerTests: XCTestCase {
    private func item(_ abbreviation: String, account: String? = nil, text: String = "T") -> SnippetItem {
        SnippetItem(id: UUID().uuidString, name: abbreviation, abbreviation: abbreviation, accountId: account, subject: nil, text: text)
    }

    func testMatchesAtTheStartOfTheTextAndAfterWhitespace() {
        let items = [item("thanks")]
        let atStart = SnippetTrigger.match(textBeforeCaret: ";thanks", in: items)
        XCTAssertEqual(atStart?.replace, NSRange(location: 0, length: 7))
        let afterSpace = SnippetTrigger.match(textBeforeCaret: "Hello\n;thanks", in: items)
        XCTAssertEqual(afterSpace?.replace, NSRange(location: 6, length: 7))
        let afterTab = SnippetTrigger.match(textBeforeCaret: "a\t;thanks", in: items)
        XCTAssertEqual(afterTab?.replace, NSRange(location: 2, length: 7))
    }

    func testDoesNotMatchMidWordOrOtherPunctuation() {
        let items = [item("b"), item("thanks")]
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: "a;b", in: items), "a;b is not a trigger")
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: "see http://x.test/a;thanks", in: items))
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: "x,;thanks", in: items), "only whitespace may precede the ;")
    }

    func testUnknownAbbreviationAndPlainTextDoNotMatch() {
        let items = [item("thanks")]
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: ";nope", in: items))
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: "thanks", in: items), "no semicolon")
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: ";", in: items))
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: "", in: items))
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: ";thanks ", in: items), "the caret must be right after the token")
    }

    func testCaseIsFolded() {
        let items = [item("thanks")]
        XCTAssertNotNil(SnippetTrigger.match(textBeforeCaret: ";THANKS", in: items))
        XCTAssertNotNil(SnippetTrigger.match(textBeforeCaret: ";Thanks", in: items))
    }

    func testRangeIsInUTF16WithEmojiBefore() {
        let match = SnippetTrigger.match(textBeforeCaret: "😀 ;ty", in: [item("ty")])
        XCTAssertEqual(match?.replace, NSRange(location: 3, length: 3), "the emoji is two UTF-16 units")
    }

    func testOnlyTheOfferedItemsCanMatchSoAccountScopingIsTheCallers() {
        var lib = TemplateLibrary()
        var mine = lib.add(name: "M", accountId: "a"); mine.abbreviation = "m"; lib.update(mine)
        XCTAssertNotNil(SnippetTrigger.match(textBeforeCaret: ";m", in: lib.snippets(for: "a")))
        XCTAssertNil(SnippetTrigger.match(textBeforeCaret: ";m", in: lib.snippets(for: "b")))
    }

    func testTheSwitchIsOffByDefault() {
        XCTAssertFalse(TemplateLibrary().inlineTriggerEnabled)
        XCTAssertFalse(try JSONDecoder().decode(TemplateLibrary.self, from: Data("{}".utf8)).inlineTriggerEnabled)
        var lib = TemplateLibrary()
        lib.inlineTriggerEnabled = true
        XCTAssertTrue(try JSONDecoder().decode(TemplateLibrary.self, from: JSONEncoder().encode(lib)).inlineTriggerEnabled)
    }
}

@MainActor
final class SnippetTabExpansionTests: XCTestCase {
    private func harness(text: String, caret: Int? = nil) -> (ComposeBodyHandle, NSTextView) {
        let tv = NSTextView(frame: NSRect(x: 0, y: 0, width: 300, height: 100))
        tv.string = text
        tv.setSelectedRange(NSRange(location: caret ?? (text as NSString).length, length: 0))
        let handle = ComposeBodyHandle()
        handle.textView = tv
        return (handle, tv)
    }

    private func items(_ abbreviation: String, text: String) -> [SnippetItem] {
        [SnippetItem(id: "1", name: "n", abbreviation: abbreviation, accountId: nil, subject: nil, text: text)]
    }

    private func render(_ item: SnippetItem) -> RenderedTemplate {
        TemplateFormatter.render(item.text, context: TemplateContext(), mode: .multiLine)
    }

    func testExpandsTheTokenBeforeTheCaretAndPlacesTheCaret() {
        let (handle, tv) = harness(text: "Hi\n;ty")
        XCTAssertTrue(handle.expandInlineSnippet(items: items("ty", text: "Thank you,${cursor} bye"), render: render))
        XCTAssertEqual(tv.string, "Hi\nThank you, bye")
        XCTAssertEqual(tv.selectedRange(), NSRange(location: 3 + "Thank you,".utf16.count, length: 0))
    }

    func testNoMatchLeavesTheTextAndTheTabAlone() {
        let (handle, tv) = harness(text: "plain text")
        XCTAssertFalse(handle.expandInlineSnippet(items: items("ty", text: "x"), render: render), "Tab must pass through")
        XCTAssertEqual(tv.string, "plain text")
    }

    func testAMidWordSemicolonIsNotExpanded() {
        let (handle, tv) = harness(text: "a;ty")
        XCTAssertFalse(handle.expandInlineSnippet(items: items("ty", text: "x"), render: render))
        XCTAssertEqual(tv.string, "a;ty")
    }

    func testWithASelectionNothingIsExpanded() {
        let (handle, tv) = harness(text: ";ty")
        tv.setSelectedRange(NSRange(location: 0, length: 3))
        XCTAssertFalse(handle.expandInlineSnippet(items: items("ty", text: "x"), render: render))
        XCTAssertEqual(tv.string, ";ty")
    }

    func testOnlyTextBeforeTheCaretCounts() {
        let (handle, tv) = harness(text: ";ty and more", caret: 3)
        XCTAssertTrue(handle.expandInlineSnippet(items: items("ty", text: "OK"), render: render))
        XCTAssertEqual(tv.string, "OK and more")
    }

    func testNoTextViewMeansNoExpansion() {
        let handle = ComposeBodyHandle()
        XCTAssertFalse(handle.expandInlineSnippet(items: items("ty", text: "x"), render: render))
    }
}
