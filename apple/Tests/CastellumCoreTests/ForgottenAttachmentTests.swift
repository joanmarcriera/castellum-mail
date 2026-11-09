// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// HP5a: table-driven tests for the pure detector.
final class ForgottenAttachmentTests: XCTestCase {
    private func word(_ body: String, subject: String = "") -> String? {
        ForgottenAttachment.triggerWord(subject: subject, body: body)
    }

    func testTriggersInEnglishSpanishAndCatalan() {
        let cases: [(String, String)] = [
            ("Please see the attached report.", "attached"),
            ("My attachment is below", "attachment"),
            ("Two attachments follow", "attachments"),
            ("I am attaching the file", "attaching"),
            ("Enclosed you will find it", "Enclosed"),
            ("Te envío el archivo adjunto.", "adjunto"),
            ("Va la factura adjunta.", "adjunta"),
            ("Los documentos adjuntos", "adjuntos"),
            ("Las fotos adjuntas", "adjuntas"),
            ("Ya adjunté el contrato", "adjunté"),
            ("Ya he adjuntado el contrato", "adjuntado"),
            ("Ver anexo 3", "anexo"),
            ("Et trameto l'adjunt", "adjunt"),
            ("Trobaràs la factura adjunta", "adjunta"),
            ("Els fitxers adjunts", "adjunts"),
            ("Les fotos adjuntes", "adjuntes"),
            ("Ho he adjuntat aquí", "adjuntat"),
            ("Vegeu l'annex 2", "annex"),
            ("Us adjuntem el contracte signat", "adjuntem"),
            ("Les adjuntamos la factura", "adjuntamos"),
        ]
        for (text, expected) in cases {
            XCTAssertEqual(word(text), expected, text)
        }
    }

    func testCaseAndDiacriticVariants() {
        XCTAssertEqual(word("ATTACHED"), "ATTACHED")
        XCTAssertEqual(word("Adjunte el archivo"), "Adjunte")      // accent dropped
        XCTAssertEqual(word("ADJUNTÉ todo"), "ADJUNTÉ")
        XCTAssertEqual(word("adjunté"), "adjunté")
        XCTAssertEqual(word("ANEXO"), "ANEXO")
    }

    func testSubjectIsScanned() {
        XCTAssertEqual(word("Hello", subject: "Invoice attached"), "attached")
    }

    func testWordBoundaries() {
        XCTAssertNil(word("The file is unattached."))
        XCTAssertNil(word("Reattachment is not a thing here"))
        XCTAssertNil(word("detached house, annexed land, adjuntador"))
        XCTAssertNil(word("Pots adjuntar el fitxer?"))
        XCTAssertEqual(word("see (attached), thanks"), "attached")
        XCTAssertEqual(word("see attached."), "attached")
    }

    func testForwardedOriginalIsNotScanned() {
        let original = MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb",
                subject: "Report", fromName: "Ada", fromAddress: "ada@x.test",
                sentAt: nil, snippet: nil, hasAttachments: false, hasBody: true,
                isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: "<o@x.test>",
            addresses: [Address(kind: "from", name: "Ada", address: "ada@x.test")],
            textBody: "Please see the attached file.", sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
        let p = ReplyPrefill.make(detail: original, mode: .forward, selfEmail: "me@x.test")
        XCTAssertNil(ForgottenAttachment.triggerWord(subject: p.subject, body: "FYI" + p.body))
        XCTAssertEqual(
            ForgottenAttachment.triggerWord(subject: p.subject, body: "See attached" + p.body),
            "attached")
        XCTAssertNil(word("Hi\n\nBegin forwarded message:\n\nthe attached file"))
    }

    func testNoTriggerInPlainText() {
        XCTAssertNil(word("Hello, see you on Thursday."))
        XCTAssertNil(word(""))
    }

    func testQuotedLinesAreIgnored() {
        XCTAssertNil(word("Thanks!\n\n> please find the attachment\n> attached"))
        XCTAssertEqual(word("Attached is the file\n> old attached"), "Attached")
    }

    func testEverythingBelowAnAttributionLineIsIgnored() {
        XCTAssertNil(word("Thanks.\n\nOn Mon, 1 Oct 2026, Ada wrote:\nsee attached\nmore attachment"))
        XCTAssertNil(word("Gracias.\n\nEl lun, 1 oct 2026, Ada escribió:\nver adjunto"))
        XCTAssertNil(word("Gràcies.\n\nEl dl., 1 d'oct. 2026, Ada va escriure:\nveure adjunt"))
        // The app's own attribution shape: "<who> wrote:" followed by a quote.
        XCTAssertNil(word("Thanks.\n\nAda wrote:\n> hi\nattached"))
        // A user's own sentence ending in "wrote:" is not an attribution.
        XCTAssertEqual(word("Here is what she wrote:\nsee attached"), "attached")
    }

    func testSignatureIsIgnored() {
        XCTAssertNil(word("Best,\nMarc\n\n-- \nSent with attachment support\nattached"))
        XCTAssertNil(word("Hi\n--\nattached"))
        XCTAssertEqual(word("attached\n\n-- \nMarc"), "attached")
    }

    func testReplyWithSignatureAndQuote() {
        let body = "Done, see attached.\n\n-- \nMarc\n\nOn Mon, Ada wrote:\n> attachment"
        XCTAssertEqual(word(body), "attached")
        let noWord = "Done.\n\n-- \nMarc\n\nOn Mon, Ada wrote:\n> attachment"
        XCTAssertNil(word(noWord))
    }
}
