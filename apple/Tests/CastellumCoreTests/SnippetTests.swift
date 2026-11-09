// Castellum - snippets (MM-7, slice 2): the library rules, where an
// insertion lands, the picker's ranking, the compose session's side, and the commands.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class SnippetLibraryTests: XCTestCase {
    func testAccountScopingShowsGlobalAndOwnNeverAnotherAccounts() {
        var lib = TemplateLibrary()
        lib.add(name: "Everywhere", accountId: nil, text: "x")
        lib.add(name: "Mine", accountId: "a", text: "x")
        lib.add(name: "Theirs", accountId: "b", text: "x")
        XCTAssertEqual(lib.snippets(for: "a").map(\.name), ["Everywhere", "Mine"])
        XCTAssertEqual(lib.snippets(for: "b").map(\.name), ["Everywhere", "Theirs"])
        XCTAssertEqual(lib.snippets(for: "c").map(\.name), ["Everywhere"])
    }

    func testDuplicateAbbreviationIsRefusedIgnoringCaseAndAccount() {
        var lib = TemplateLibrary()
        var one = lib.add(name: "One", accountId: "a")
        var two = lib.add(name: "Two", accountId: "b")
        one.abbreviation = "thanks"
        XCTAssertTrue(lib.update(one).isEmpty)
        two.abbreviation = "THANKS".lowercased()
        XCTAssertFalse(lib.update(two).isEmpty, "the same trigger in another account is still ambiguous")
        XCTAssertNil(lib.snippet(id: two.id)?.abbreviation, "a refused edit changes nothing")
        // Editing the snippet that owns it is fine.
        one.name = "One renamed"
        XCTAssertTrue(lib.update(one).isEmpty)
    }

    func testLimitsOnNameAbbreviationTextAndSubject() {
        var lib = TemplateLibrary()
        var s = lib.add(name: "Ok", accountId: nil)
        s.name = "  "
        XCTAssertFalse(lib.validate(s).isEmpty, "a blank name")
        s.name = String(repeating: "n", count: 81)
        XCTAssertFalse(lib.validate(s).isEmpty, "81 characters")
        s.name = String(repeating: "n", count: 80)
        XCTAssertTrue(lib.validate(s).isEmpty)
        for bad in ["Has Space", "UPPER", "semi;colon", String(repeating: "a", count: 25), "é"] {
            s.abbreviation = bad
            XCTAssertFalse(lib.validate(s).isEmpty, bad)
        }
        for good in ["thanks", "a-1", String(repeating: "a", count: 24)] {
            s.abbreviation = good
            XCTAssertTrue(lib.validate(s).isEmpty, good)
        }
        s.abbreviation = nil
        s.text = String(repeating: "x", count: 64 * 1024)
        XCTAssertTrue(lib.validate(s).isEmpty)
        s.text += "x"
        XCTAssertFalse(lib.validate(s).isEmpty, "over 64 KiB")
        s.text = ""
        s.subject = "line one\nBcc: evil@x.test"
        XCTAssertFalse(lib.validate(s).isEmpty, "a subject is one line")
    }

    func testAbbreviationLookupPrefersTheAccountsOwnAndIgnoresCase() {
        var lib = TemplateLibrary()
        var global = lib.add(name: "G", accountId: nil)
        global.abbreviation = "ty"
        lib.update(global)
        XCTAssertEqual(lib.snippet(abbreviation: "TY", accountId: "a")?.id, global.id)
        XCTAssertNil(lib.snippet(abbreviation: "nope", accountId: "a"))
        var mine = lib.add(name: "M", accountId: "a")
        mine.abbreviation = "mine"
        lib.update(mine)
        XCTAssertNil(lib.snippet(abbreviation: "mine", accountId: "b"), "another account's snippet is not offered")
    }

    func testSuggestedNamesAreUniquePerScope() {
        var lib = TemplateLibrary()
        XCTAssertEqual(lib.suggestedName(accountId: nil, base: "Snippet"), "Snippet")
        lib.add(name: "Snippet", accountId: nil)
        XCTAssertEqual(lib.suggestedName(accountId: nil, base: "Snippet"), "Snippet 2")
        XCTAssertEqual(lib.suggestedName(accountId: "a", base: "Snippet"), "Snippet", "another scope")
    }

    func testCodableRoundTripKeepsSnippetsAndPruneDropsOnlyAccountScopedOnes() throws {
        var lib = TemplateLibrary()
        lib.add(name: "G", accountId: nil, text: "g")
        lib.add(name: "A", accountId: "a", text: "a")
        lib.add(name: "B", accountId: "b", text: "b")
        let back = try JSONDecoder().decode(TemplateLibrary.self, from: JSONEncoder().encode(lib))
        XCTAssertEqual(back, lib)
        lib.prune(keepingAccounts: ["a"])
        XCTAssertEqual(Set(lib.snippets.map(\.name)), ["G", "A"])
    }

    func testDeleteRemovesTheSnippet() {
        var lib = TemplateLibrary()
        let s = lib.add(name: "Gone", accountId: nil)
        lib.delete(id: s.id)
        XCTAssertNil(lib.snippet(id: s.id))
    }
}

final class SnippetInsertionTests: XCTestCase {
    private func rendered(_ text: String, caret: Int? = nil) -> RenderedTemplate {
        RenderedTemplate(text: text, caretOffset: caret)
    }

    func testInsertsAtTheCaret() {
        let r = SnippetInsertion.apply(rendered("XY"), to: "ab", selection: NSRange(location: 1, length: 0))
        XCTAssertEqual(r.body, "aXYb")
        XCTAssertEqual(r.caret, 3, "after the inserted text when there is no cursor marker")
    }

    func testReplacesASelection() {
        let r = SnippetInsertion.apply(rendered("Z"), to: "hello world", selection: NSRange(location: 0, length: 5))
        XCTAssertEqual(r.body, "Z world")
    }

    func testCaretLandsAtTheCursorMarker() {
        let r = SnippetInsertion.apply(rendered("Hi , bye", caret: 3), to: "", selection: NSRange(location: 0, length: 0))
        XCTAssertEqual(r.body, "Hi , bye")
        XCTAssertEqual(r.caret, 3)
    }

    func testNoUsableSelectionGoesToTheEndOfTheAuthoredText() {
        let r = SnippetInsertion.apply(rendered("!"), to: "abc", selection: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(r.body, "abc!")
        let past = SnippetInsertion.apply(rendered("!"), to: "abc", selection: NSRange(location: 99, length: 0))
        XCTAssertEqual(past.body, "abc!")
    }

    func testCaretOffsetCountsUTF16() {
        // An emoji is two UTF-16 units; the marker after it sits at 2 + 1.
        let r = SnippetInsertion.apply(rendered("😀 x", caret: 3), to: "", selection: NSRange(location: 0, length: 0))
        XCTAssertEqual(r.caret, 3)
    }

    func testNeverInsertsInsideTheQuoteOfAReply() {
        let body = "\n\nOn Mon, Ada wrote:\n> quoted text"
        // A caret placed inside the quote is clamped above it.
        let inside = (body as NSString).range(of: "quoted").location
        let r = SnippetInsertion.apply(rendered("SNIP"), to: body, selection: NSRange(location: inside, length: 0))
        XCTAssertTrue(r.body.hasPrefix("SNIP\n\nOn Mon"), r.body)
        XCTAssertTrue(r.body.hasSuffix("> quoted text"))
    }

    func testASelectionReachingIntoTheQuoteIsTruncatedAtTheAuthoredText() {
        let body = "Dear Ada\n\nOn Mon, Ada wrote:\n> quoted"
        let r = SnippetInsertion.apply(rendered("Hi"), to: body, selection: NSRange(location: 5, length: body.utf16.count - 5))
        XCTAssertTrue(r.body.hasPrefix("Dear Hi\n\nOn Mon"), r.body)
        XCTAssertTrue(r.body.hasSuffix("> quoted"), "the quote is untouched")
    }

    func testNeverInsertsInsideTheSignature() {
        let body = "Hello\n\n-- \nMarc"
        let r = SnippetInsertion.apply(rendered("S"), to: body, selection: NSRange(location: body.utf16.count, length: 0))
        XCTAssertEqual(r.body, "HelloS\n\n-- \nMarc")
    }

    func testBlankNewMessageWithASignatureInsertsAtTheTop() {
        let body = "\n\n-- \nMarc"
        let r = SnippetInsertion.apply(rendered("Hi"), to: body, selection: NSRange(location: 0, length: 0))
        XCTAssertEqual(r.body, "Hi\n\n-- \nMarc")
    }
}

final class SnippetPickerLogicTests: XCTestCase {
    func testEmptyQueryKeepsTheLibraryOrderAndAQueryFiltersByNameOrAbbreviation() {
        var lib = TemplateLibrary()
        lib.add(name: "Thanks and regards", accountId: nil)
        var out = lib.add(name: "Out of office", accountId: nil)
        out.abbreviation = "ooo"
        lib.update(out)
        let all = lib.snippets(for: "a")
        XCTAssertEqual(SnippetPickerLogic.ranked(all, query: "").map(\.name), ["Out of office", "Thanks and regards"])
        XCTAssertEqual(SnippetPickerLogic.ranked(all, query: "thx").map(\.name), [], "no subsequence 'thx'")
        XCTAssertEqual(SnippetPickerLogic.ranked(all, query: "tnr").map(\.name), ["Thanks and regards"])
        XCTAssertEqual(SnippetPickerLogic.ranked(all, query: "ooo").map(\.name), ["Out of office"], "by abbreviation")
    }
}

@MainActor
final class SnippetComposeSessionTests: XCTestCase {
    private func session(to: String = "") -> ComposeSession {
        let s = ComposeSession(accountId: "acct")
        s.toText = to
        return s
    }

    private func snippet(_ text: String) -> SnippetItem {
        SnippetItem(id: "s", name: "S", abbreviation: nil, accountId: nil, subject: nil, text: text)
    }

    func testInsertingMakesTheSessionEdited() {
        let s = session()
        XCTAssertFalse(s.hasUserEdits)
        s.insertSnippet(snippet("Hello"), context: s.snippetContext(selfEmail: "me@x.test", selfName: "Me"))
        XCTAssertEqual(s.body, "Hello")
        XCTAssertTrue(s.hasUserEdits)
        XCTAssertEqual(s.closeAction, .save)
    }

    func testContextComesFromTheFirstToTheSubjectAndTheFromIdentity() {
        let s = session(to: "Grace Hopper <grace@x.test>, other@x.test")
        s.subject = "Plans"
        let ctx = s.snippetContext(selfEmail: "me@x.test", selfName: "Me")
        XCTAssertEqual(ctx.recipientName, "Grace Hopper")
        XCTAssertEqual(ctx.recipientEmail, "grace@x.test")
        XCTAssertEqual(ctx.subject, "Plans")
        XCTAssertEqual(ctx.myEmail, "me@x.test")
        XCTAssertEqual(ctx.myName, "Me")
        s.userChoseFrom(ComposeIdentity(email: "alias@x.test", name: "Alias"))
        let aliased = s.snippetContext(selfEmail: "me@x.test", selfName: "Me")
        XCTAssertEqual(aliased.myEmail, "alias@x.test")
        XCTAssertEqual(aliased.myName, "Alias")
    }

    func testPlaceholdersAreFilledAndInjectionStaysOnOneLine() {
        let s = session(to: "Grace Hopper <grace@x.test>")
        s.insertSnippet(snippet("Dear ${recipient.first},${cursor}"), context: s.snippetContext(selfEmail: "me@x.test", selfName: nil))
        XCTAssertEqual(s.body, "Dear Grace,")
    }
}

final class SnippetCommandTests: XCTestCase {
    func testInsertSnippetIsRegisteredWithOptionCommandI() {
        let spec = CommandRegistry.spec(for: .insertSnippet)
        XCTAssertEqual(spec.notification, .castellumInsertSnippet)
        XCTAssertEqual(spec.menuChord?.display, "⌥⌘I")
        XCTAssertFalse(spec.destructive, "it must fire while the body has focus")
        XCTAssertEqual(CommandRegistry.spec(for: .showSnippets).notification, .castellumShowSnippets)
        XCTAssertTrue(CommandRegistry.conflicts(in: CommandRegistry.all).isEmpty)
    }

    func testInsertSnippetNeedsAnOpenComposer() {
        var ctx = PaletteContext(hasSelectedMessage: true, messageBodyAvailable: true, aiEnabled: false)
        XCTAssertFalse(CommandPaletteAvailability.isEnabled(.insertSnippet, context: ctx))
        ctx.composerOpen = true
        XCTAssertTrue(CommandPaletteAvailability.isEnabled(.insertSnippet, context: ctx))
        XCTAssertTrue(CommandPaletteAvailability.isEnabled(.showSnippets, context: PaletteContext(
            hasSelectedMessage: false, messageBodyAvailable: false, aiEnabled: false)))
    }

    @MainActor
    func testThePaletteRowPostsTheInsertSnippetNotification() {
        var posted: [String] = []
        let done = expectation(description: "posted")
        let executor = CommandPaletteExecutor(dismiss: {}, post: { posted.append($0.rawValue); done.fulfill() })
        executor.execute(CommandRegistry.spec(for: .insertSnippet))
        wait(for: [done], timeout: 1)
        XCTAssertEqual(posted, ["castellum.insertSnippet"])
    }
}
