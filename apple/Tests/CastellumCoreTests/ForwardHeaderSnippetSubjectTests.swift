// Castellum - MM-7 slice 4: the per-account forward header format and a
// snippet's subject. The built-in forward is byte-identical without a format; the
// separator line is fixed; a snippet subject fills only an EMPTY Subject and never carries a line break.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class ForwardHeaderTests: XCTestCase {
    private func detail(subject: String? = "Plans", sentAt: Int64? = 1_792_139_400,
                        fromName: String? = "Ada Lovelace", attachments: Int = 0) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb", subject: subject,
                fromName: fromName, fromAddress: "ada@x.test", sentAt: sentAt, snippet: nil,
                hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: "<orig@x.test>",
            addresses: [Address(kind: "from", name: fromName, address: "ada@x.test")],
            textBody: "Original body.", sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
    }

    private func templates(_ header: String?) -> ReplyTemplates {
        ReplyTemplates(forwardHeader: header, myName: "Marc", locale: Locale(identifier: "en_GB"),
                       timeZone: TimeZone(identifier: "UTC")!, now: Date(timeIntervalSince1970: 0))
    }

    func testNoFormatIsByteIdenticalToTheBuiltInBlock() {
        let plain = ReplyPrefill.make(detail: detail(), mode: .forward, selfEmail: "me@x.test")
        let viaTemplates = ReplyPrefill.make(detail: detail(), mode: .forward, selfEmail: "me@x.test", templates: templates(nil))
        XCTAssertEqual(plain.body, viaTemplates.body)
        XCTAssertTrue(plain.body.contains("\(ForgottenAttachment.forwardedSeparator)\nFrom: Ada Lovelace <ada@x.test>\nDate: "))
        XCTAssertTrue(plain.body.hasSuffix("Subject: Plans\n\nOriginal body."))
    }

    func testCustomHeaderSitsUnderTheFixedSeparator() {
        let p = ReplyPrefill.make(
            detail: detail(), mode: .forward, selfEmail: "me@x.test",
            templates: templates("Von: ${sender.name}\nAm: ${date:d/M/yyyy}\nBetreff: ${subject}"))
        XCTAssertEqual(
            p.body,
            "\n\n\(ForgottenAttachment.forwardedSeparator)\nVon: Ada Lovelace\nAm: 16/10/2026\nBetreff: Plans\n\nOriginal body.")
        XCTAssertEqual(p.subject, "Fwd: Plans", "the subject prefix is unchanged")
        XCTAssertTrue(p.isForward)
    }

    func testAHeaderThatRendersEmptyLeavesJustTheSeparator() {
        let p = ReplyPrefill.make(
            detail: detail(sentAt: nil), mode: .forward, selfEmail: "me@x.test", templates: templates("${date}"))
        XCTAssertEqual(p.body, "\n\n\(ForgottenAttachment.forwardedSeparator)\n\nOriginal body.")
    }

    func testSenderValuesAreNotReExpanded() {
        let p = ReplyPrefill.make(
            detail: detail(fromName: "${cursor} ${my.email}"), mode: .forward, selfEmail: "me@x.test",
            templates: templates("From: ${sender.name}"))
        XCTAssertTrue(p.body.contains("From: ${cursor} ${my.email}"), "a hostile name stays literal text")
    }

    func testForgottenAttachmentStillStopsAtTheSeparator() {
        // The words below the separator belong to the forwarded original, whatever the header says.
        let p = ReplyPrefill.make(
            detail: detail(subject: "see attached file"), mode: .forward, selfEmail: "me@x.test",
            templates: templates("Betreff: ${subject}"))
        XCTAssertNil(ForgottenAttachment.triggerWord(subject: "", body: p.body))
    }

    func testReplyFormatAndForwardHeaderAreIndependent() {
        let t = ReplyTemplates(attribution: "${sender} said:", forwardHeader: "X: ${subject}")
        let reply = ReplyPrefill.make(detail: detail(), mode: .reply, selfEmail: "me@x.test", templates: t)
        XCTAssertTrue(reply.body.contains("Ada Lovelace said:"))
        XCTAssertFalse(reply.body.contains("X: Plans"))
    }

    func testLibraryStoresTheHeaderPerAccount() throws {
        var lib = TemplateLibrary()
        lib.setForwardHeader("From: ${sender}", for: "a")
        XCTAssertEqual(lib.forwardHeaderFormat(for: "a"), "From: ${sender}")
        XCTAssertNil(lib.forwardHeaderFormat(for: "b"))
        XCTAssertEqual(try JSONDecoder().decode(TemplateLibrary.self, from: JSONEncoder().encode(lib)), lib)
        lib.setForwardHeader("  ", for: "a")
        XCTAssertNil(lib.forwardHeaderFormat(for: "a"), "blank clears it")
        lib.setForwardHeader("x", for: "a")
        lib.prune(keepingAccounts: [])
        XCTAssertNil(lib.forwardHeaderFormat(for: "a"))
    }
}

@MainActor
final class SnippetSubjectTests: XCTestCase {
    private func snippet(subject: String?, text: String = "Body") -> SnippetItem {
        SnippetItem(id: "s", name: "S", abbreviation: nil, accountId: nil, subject: subject, text: text)
    }

    private func context(_ s: ComposeSession) -> TemplateContext {
        s.snippetContext(selfEmail: "me@x.test", selfName: "Me")
    }

    func testFillsAnEmptySubjectAndLeavesAFilledOneAlone() {
        let empty = ComposeSession(accountId: "acct")
        empty.toText = "Grace Hopper <grace@x.test>"
        empty.insertSnippet(snippet(subject: "Hello ${recipient.first}"), context: context(empty))
        XCTAssertEqual(empty.subject, "Hello Grace")
        XCTAssertEqual(empty.body, "Body")

        let filled = ComposeSession(accountId: "acct")
        filled.subject = "My own subject"
        filled.insertSnippet(snippet(subject: "Snippet subject"), context: context(filled))
        XCTAssertEqual(filled.subject, "My own subject")
        XCTAssertEqual(filled.body, "Body", "the body part still inserts")
    }

    func testAReplysInheritedSubjectIsNotOverwritten() {
        let s = ComposeSession(accountId: "acct", prefill: ReplyPrefill(
            to: [DraftAddress(name: nil, email: "a@x.test")], subject: "Re: Plans", inReplyTo: nil,
            referencesHeader: nil, body: "\n\nquoted"))
        s.insertSnippet(snippet(subject: "Other"), context: context(s))
        XCTAssertEqual(s.subject, "Re: Plans")
    }

    func testNoSubjectInTheSnippetLeavesTheFieldEmpty() {
        let s = ComposeSession(accountId: "acct")
        s.insertSnippet(snippet(subject: nil), context: context(s))
        XCTAssertEqual(s.subject, "")
        s.insertSnippet(snippet(subject: ""), context: context(s))
        XCTAssertEqual(s.subject, "")
    }

    func testALineBreakNeverReachesTheSubjectHeader() {
        // Validation rejects a multi-line subject in the editor; a hand-edited library or a hostile
        // value must still be flattened by the single-line render.
        let s = ComposeSession(accountId: "acct")
        s.insertSnippet(snippet(subject: "x\r\nBcc: evil@x.test"), context: context(s))
        XCTAssertFalse(s.subject.contains("\n") || s.subject.contains("\r"), s.subject)
        let hostile = TemplateContext(recipientName: "x\r\nBcc: evil@x.test")
        let t = ComposeSession(accountId: "acct")
        t.insertSnippet(snippet(subject: "Hi ${recipient.name}"), context: hostile)
        XCTAssertFalse(t.subject.contains("\n") || t.subject.contains("\r"), t.subject)
        XCTAssertTrue(t.subject.hasPrefix("Hi x"))
    }
}
