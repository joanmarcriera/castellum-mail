// Castellum - custom reply attributions end to end:
// the format reaches the reply body, the default is byte-identical, and the
// forgotten-attachment scan and signature/AI insertion cope with a custom line.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class ReplyAttributionTests: XCTestCase {
    private func detail(body: String = "See you then.", sentAt: Int64? = 1_792_139_400) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb",
                subject: "Plans", fromName: "Ada Lovelace", fromAddress: "ada@x.test",
                sentAt: sentAt, snippet: nil, hasAttachments: false, hasBody: true,
                isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: "<orig@x.test>",
            addresses: [Address(kind: "from", name: "Ada Lovelace", address: "ada@x.test")],
            textBody: body, sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
    }

    private func templates(_ format: String?) -> ReplyTemplates {
        ReplyTemplates(
            attribution: format, myName: "Marc", locale: Locale(identifier: "ca_ES"),
            timeZone: TimeZone(identifier: "UTC")!, now: Date(timeIntervalSince1970: 0))
    }

    func testNoFormatIsByteIdenticalToTheBuiltInLine() {
        let plain = ReplyPrefill.make(detail: detail(), mode: .reply, selfEmail: "me@x.test")
        let viaTemplates = ReplyPrefill.make(
            detail: detail(), mode: .reply, selfEmail: "me@x.test", templates: templates(nil))
        XCTAssertEqual(plain.body, viaTemplates.body)
        XCTAssertTrue(plain.body.hasPrefix("\n\nOn "))
        XCTAssertTrue(plain.body.contains(" Ada Lovelace wrote:\n> See you then."))
        XCTAssertEqual(plain.attributionLine, String(plain.body.dropFirst(2).split(separator: "\n")[0]))
    }

    func testCustomFormatProducesOneLineAboveTheQuote() {
        let p = ReplyPrefill.make(
            detail: detail(), mode: .reply, selfEmail: "me@x.test",
            templates: templates("El ${date:d/M/yyyy}, ${sender.name} va escriure:"))
        XCTAssertEqual(p.body, "\n\nEl 16/10/2026, Ada Lovelace va escriure:\n> See you then.")
        XCTAssertEqual(p.attributionLine, "El 16/10/2026, Ada Lovelace va escriure:")
    }

    func testAFormatThatRendersEmptyGivesNoAttributionLine() {
        let p = ReplyPrefill.make(
            detail: detail(sentAt: nil), mode: .reply, selfEmail: "me@x.test",
            templates: templates("${date}"))
        XCTAssertEqual(p.body, "\n\n> See you then.")
        XCTAssertNil(p.attributionLine)
    }

    func testInsertionsStillLandAboveACustomLine() {
        let p = ReplyPrefill.make(
            detail: detail(), mode: .reply, selfEmail: "me@x.test",
            templates: templates("${sender} said:"))
        let withSuggestion = ReplyPrefill.insertAboveQuote(suggestion: "Sounds good.", in: p.body)
        XCTAssertTrue(withSuggestion.hasPrefix("Sounds good."), withSuggestion)
        XCTAssertTrue(withSuggestion.contains("\n\nAda Lovelace said:\n> See you then."), withSuggestion)
        let signed = SignatureSupport.insert(signature: "Marc", into: p.body)
        XCTAssertTrue(signed.contains("Ada Lovelace said:\n> See you then."), signed)
        XCTAssertLessThan(
            signed.range(of: "Marc")!.lowerBound, signed.range(of: "Ada Lovelace said:")!.lowerBound)
    }

    func testForwardIsUnaffectedByTheReplyFormat() {
        let p = ReplyPrefill.make(
            detail: detail(), mode: .forward, selfEmail: "me@x.test", templates: templates("X"))
        XCTAssertNil(p.attributionLine)
        XCTAssertTrue(p.body.contains(ForgottenAttachment.forwardedSeparator))
    }

    func testForgottenAttachmentIgnoresAKnownCustomAttributionLine() {
        // The line has no "wrote:" suffix and contains a trigger word.
        let line = "Attachment Team said, attached below:"
        let body = "Thanks!\n\n\(line)\n> original text"
        XCTAssertNotNil(ForgottenAttachment.triggerWord(subject: "", body: body),
                        "without the known line the attribution words are scanned")
        XCTAssertNil(ForgottenAttachment.triggerWord(subject: "", body: body, knownAttribution: line))
        // The user's own words above it still count.
        XCTAssertEqual(
            ForgottenAttachment.triggerWord(
                subject: "", body: "See attached.\n\n\(line)\n> x", knownAttribution: line),
            "attached")
    }
}
