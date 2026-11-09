// Castellum - placeholder formatter for reply attributions (MM-7, #2697).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class TemplateFormatterTests: XCTestCase {
    /// 2026-10-16 08:30:00 UTC.
    private let sent: Int64 = 1_792_139_400

    private func context(
        locale: String = "en_GB", sentAt: Int64? = 1_792_139_400, senderName: String? = "Ada Lovelace"
    ) -> TemplateContext {
        TemplateContext(
            senderName: senderName, senderEmail: "ada@example.org", sentAt: sentAt,
            subject: "Lunch?", recipientName: "Grace Hopper", recipientEmail: "grace@example.org",
            myName: "Marc", myEmail: "marc@example.org",
            now: Date(timeIntervalSince1970: 1_792_139_400 + 3600),
            locale: Locale(identifier: locale), timeZone: TimeZone(identifier: "UTC")!)
    }

    private func line(_ f: String, _ c: TemplateContext? = nil) -> String {
        TemplateFormatter.render(f, context: c ?? context(), mode: .singleLine).text
    }

    func testEveryDocumentedPlaceholderRenders() {
        XCTAssertEqual(line("${sender}"), "Ada Lovelace")
        XCTAssertEqual(line("${sender.name}"), "Ada Lovelace")
        XCTAssertEqual(line("${sender.first}"), "Ada")
        XCTAssertEqual(line("${sender.email}"), "ada@example.org")
        XCTAssertEqual(line("${subject}"), "Lunch?")
        XCTAssertEqual(line("${recipient}"), "Grace Hopper")
        XCTAssertEqual(line("${recipient.name}"), "Grace Hopper")
        XCTAssertEqual(line("${recipient.first}"), "Grace")
        XCTAssertEqual(line("${recipient.email}"), "grace@example.org")
        XCTAssertEqual(line("${my.name}"), "Marc")
        XCTAssertEqual(line("${my.email}"), "marc@example.org")
        XCTAssertFalse(line("${date}").isEmpty)
        XCTAssertFalse(line("${today}").isEmpty)
    }

    func testSenderFallsBackToTheAddress() {
        XCTAssertEqual(line("${sender}", context(senderName: nil)), "ada@example.org")
        XCTAssertEqual(line("${sender}", context(senderName: "  ")), "ada@example.org")
    }

    func testDatePatternsFollowTheInjectedLocale() {
        XCTAssertEqual(line("${date:d MMM yyyy HH:mm}"), "16 Oct 2026 08:30")
        XCTAssertEqual(line("${date:d/M/yyyy}", context(locale: "ca_ES")), "16/10/2026")
        XCTAssertTrue(line("${date:d MMMM yyyy}", context(locale: "en_GB")).contains("October"))
        XCTAssertTrue(line("${date:d MMMM yyyy}", context(locale: "es_ES")).contains("octubre"))
        XCTAssertTrue(line("${date:d MMMM yyyy}", context(locale: "ca_ES")).contains("octubre"))
        XCTAssertEqual(line("${today:HH:mm}"), "09:30")
    }

    func testFallbackAppliesOnlyWhenTheValueIsEmpty() {
        XCTAssertEqual(line("${sender.name|someone}", context(senderName: nil)), "someone")
        XCTAssertEqual(line("${sender.name|someone}"), "Ada Lovelace")
        XCTAssertEqual(line("On ${date|an unknown day}, ${sender} wrote:", context(sentAt: nil)),
                       "On an unknown day, Ada Lovelace wrote:")
    }

    func testEscapeAndUnclosedBrace() {
        XCTAssertEqual(line("cost $${sender}"), "cost ${sender}")
        XCTAssertEqual(line("broken ${sender"), "broken ${sender")
        XCTAssertEqual(line("$ alone and ${"), "$ alone and ${")
    }

    func testUnknownPlaceholderIsLeftAsTypedAndFlagged() {
        XCTAssertEqual(line("hi ${sendr}"), "hi ${sendr}")
        let problems = TemplateFormatter.problems(in: "hi ${sendr} ${date:x}", mode: .singleLine)
        XCTAssertEqual(problems.map(\.source), ["${sendr}"])
        XCTAssertEqual(TemplateFormatter.problems(in: "${sender:oops}", mode: .singleLine).count, 1)
        XCTAssertEqual(TemplateFormatter.problems(in: "${sender", mode: .singleLine).count, 1)
    }

    func testValuesAreNeverReExpanded() {
        let c = context(senderName: "${cursor}${my.email}")
        XCTAssertEqual(line("${sender}", c), "${cursor}${my.email}")
    }

    func testLineBreaksInFormatAndValuesBecomeASpaceAndWhitespaceCollapses() {
        let c = context(senderName: "Ada\r\nLovelace\u{2028}Jr")
        XCTAssertEqual(line("  A\nB   ${sender}  ", c), "A B Ada Lovelace Jr")
    }

    func testEmptyRenderIsEmptyAndLongValuesAreCapped() {
        XCTAssertEqual(line("${sender.name}", context(senderName: nil)), "")
        let long = String(repeating: "x", count: 5_000)
        XCTAssertEqual(line("${sender}", context(senderName: long)).count, TemplateFormatter.maxValueLength)
    }

    func testCursorMarksTheCaretInUTF16AndIsAProblemInSingleLineMode() {
        let r = TemplateFormatter.render("😀 Hi ${recipient.first},\n${cursor}\nBye", context: context(), mode: .multiLine)
        XCTAssertEqual(r.text, "😀 Hi Grace,\n\nBye")
        XCTAssertEqual(r.caretOffset, ("😀 Hi Grace,\n" as NSString).length, "UTF-16, emoji counts as 2")
        XCTAssertEqual(TemplateFormatter.problems(in: "${cursor}", mode: .singleLine).count, 1)
        XCTAssertEqual(TemplateFormatter.problems(in: "${cursor}", mode: .multiLine).count, 0)
        XCTAssertNil(TemplateFormatter.render("a${cursor}", context: context(), mode: .singleLine).caretOffset)
    }

    func testAHugeFormatRendersQuickly() {
        let format = String(repeating: "x ${sender} ", count: 6_000)   // ~64 KiB
        let start = Date()
        _ = TemplateFormatter.render(format, context: context(), mode: .singleLine)
        XCTAssertLessThan(Date().timeIntervalSince(start), 1.0)
    }

    func testPlaceholderDocFileListsEveryPlaceholder() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().appendingPathComponent("docs/template-placeholders.md")
        let doc = try String(contentsOf: url, encoding: .utf8)
        for p in TemplateFormatter.placeholders {
            XCTAssertTrue(doc.contains("${\(p.name)}"), "template-placeholders.md misses \(p.name)")
        }
    }
}
