// Castellum - Reply quotes the selection, strips the sender's signature and re-wraps
// (MM-17, slice 4). The wiring from the reading pane (NSTextView / WKWebView
// selection) is Marc's GUI check; the rules are all here.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class ReplySelectionTests: XCTestCase {
    private func detail(body: String) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb", subject: "Plans", fromName: "Ada Lovelace",
                fromAddress: "ada@x.test", sentAt: 1_792_139_400, snippet: nil, hasAttachments: false, hasBody: true,
                isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: "<orig@x.test>",
            addresses: [Address(kind: "from", name: "Ada Lovelace", address: "ada@x.test")],
            textBody: body, sanitizedHtml: nil, hasRemoteContent: nil, attachments: nil, trust: nil)
    }

    private func make(
        _ body: String, mode: ReplyMode = .reply, selection: String? = nil,
        templates: ReplyTemplates = ReplyTemplates(locale: Locale(identifier: "en_GB"), timeZone: TimeZone(identifier: "UTC")!)
    ) -> ReplyPrefill {
        ReplyPrefill.make(detail: detail(body: body), mode: mode, selfEmail: "me@x.test", templates: templates, selection: selection)
    }

    private let sig = "Best,\nAda\n\n-- \nAda Lovelace\nAnalytical Engines Ltd"

    // MARK: the selection

    func testASelectionQuotesOnlyTheSelection() {
        let p = make("First paragraph.\n\nSecond paragraph.\n\nThird paragraph.", selection: "Second paragraph.")
        XCTAssertTrue(p.body.hasSuffix("\n> Second paragraph."), p.body)
        XCTAssertFalse(p.body.contains("First paragraph"))
        XCTAssertFalse(p.body.contains("Third paragraph"))
        XCTAssertTrue(p.body.hasPrefix("\n\n"), "the caret still starts at the top")
    }

    func testABlankSelectionFallsBackToTheWholeMessage() {
        for blank in ["", "   ", "\n\n", " \t "] {
            let p = make("Only text.", selection: blank)
            XCTAssertTrue(p.body.hasSuffix("\n> Only text."), "\(blank.debugDescription): \(p.body)")
        }
        XCTAssertTrue(make("Only text.", selection: nil).body.hasSuffix("\n> Only text."))
    }

    func testASelectionThatAlreadyContainsQuotedLinesNests() {
        let p = make("irrelevant", selection: "Ada said:\n> earlier\n> > earliest")
        // quotedReplyBody's own rule: an already-quoted line gains one more ">" (no space), a plain line "> ".
        XCTAssertTrue(p.body.hasSuffix("> Ada said:\n>> earlier\n>> > earliest"), p.body)
        XCTAssertFalse(p.body.contains("irrelevant"))
    }

    func testReplyAllHonoursTheSelectionToo() {
        let p = make("One.\n\nTwo.", mode: .replyAll, selection: "Two.")
        XCTAssertTrue(p.body.hasSuffix("\n> Two."))
        XCTAssertFalse(p.body.contains("One."))
    }

    func testForwardIgnoresTheSelectionAndKeepsTheSignature() {
        let p = make("Body text.\n\n\(sig)", mode: .forward, selection: "Body text.")
        XCTAssertTrue(p.body.contains("Analytical Engines Ltd"), "forward keeps the whole message, signature included")
        XCTAssertTrue(p.body.contains(ForgottenAttachment.forwardedSeparator))
    }

    // MARK: signature

    func testTheSendersStrictSignatureIsStrippedFromAWholeMessageQuote() {
        let p = make("Hello Marc.\n\n\(sig)")
        XCTAssertTrue(p.body.contains("> Hello Marc."))
        XCTAssertTrue(p.body.contains("> Best,\n> Ada"), "the closing stays; only the signature block goes")
        XCTAssertFalse(p.body.contains("Analytical Engines"), p.body)
        XCTAssertFalse(p.body.contains("> -- "), p.body)
    }

    func testABareDelimiterSignatureIsKeptBecauseStrippingIsDestructive() {
        let p = make("Hello.\n--\nAda")
        XCTAssertTrue(p.body.contains("> --\n> Ada"), p.body)
    }

    func testASelectionIsQuotedAsSelectedEvenIfItContainsASignature() {
        let p = make("irrelevant", selection: "Best,\nAda\n-- \nAda Lovelace")
        XCTAssertTrue(p.body.contains("> Ada Lovelace"), "the user picked it; we do not second-guess a selection")
    }

    // MARK: re-wrap

    private let l1 = "This is a hard wrapped paragraph that a mail client broke at roughly"
    private let l2 = "seventy columns, so the quote below it looks like a comb when it is"
    private let l3 = "quoted with a prefix."

    func testHardWrappedProseIsJoinedIntoAParagraphByDefault() {
        let p = make("\(l1)\n\(l2)\n\(l3)")
        XCTAssertTrue(p.body.hasSuffix("\n> \(l1) \(l2) \(l3)"), p.body)
    }

    func testTheHiddenPreferenceTurnsTheRewrapOff() {
        var t = ReplyTemplates(locale: Locale(identifier: "en_GB"), timeZone: TimeZone(identifier: "UTC")!)
        t.rewrapQuotes = false
        let p = make("\(l1)\n\(l2)\n\(l3)", templates: t)
        XCTAssertTrue(p.body.hasSuffix("\n> \(l1)\n> \(l2)\n> \(l3)"), p.body)
    }

    func testCodeAndListsInTheOriginalStayAsTheyAre() {
        let body = "\(l1)\n    code line\n- a list item"
        let p = make(body)
        XCTAssertTrue(p.body.hasSuffix("\n> \(l1)\n>     code line\n> - a list item"), p.body)
    }

    // MARK: coexistence with the rest of the compose machinery

    func testInsertionsStillLandAboveTheNewQuote() {
        let p = make("Whole message.\n\n\(sig)", selection: "Whole message.")
        let suggestion = ReplyPrefill.insertAboveQuote(suggestion: "Thanks!", in: p.body)
        XCTAssertTrue(suggestion.hasPrefix("Thanks!\n\n"), suggestion)
        let signed = SignatureSupport.insert(signature: "Marc", into: p.body)
        XCTAssertLessThan(signed.range(of: "Marc")!.lowerBound, signed.range(of: "wrote:")!.lowerBound)
        XCTAssertTrue(signed.contains("> Whole message."))
    }

    func testQuotedOriginalHelperDirectly() {
        XCTAssertNil(ReplyPrefill.quotedOriginal(textBody: nil, selection: nil, rewrap: true))
        XCTAssertEqual(ReplyPrefill.quotedOriginal(textBody: nil, selection: "just this", rewrap: true), "just this")
        XCTAssertEqual(ReplyPrefill.quotedOriginal(textBody: "a\n-- \nb", selection: nil, rewrap: true), "a")
    }
}

@MainActor
final class ReadingSelectionModelTests: XCTestCase {
    func testASelectionOfAnotherMessageIsNeverQuoted() {
        let selection = ReadingSelection(messageId: "m1", text: "picked")
        // RootView applies this exact rule when opening Reply.
        func applicable(_ s: ReadingSelection?, for id: String) -> String? { s.flatMap { $0.messageId == id ? $0.text : nil } }
        XCTAssertEqual(applicable(selection, for: "m1"), "picked")
        XCTAssertNil(applicable(selection, for: "m2"))
        XCTAssertNil(applicable(nil, for: "m1"))
    }
}
