// Castellum - header display modes.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class MessageHeaderModeTests: XCTestCase {
    func testEachModeShowsMoreThanTheLast() {
        XCTAssertEqual(MessageHeaderMode.compact.recipientKinds, [])
        XCTAssertEqual(MessageHeaderMode.extended.recipientKinds, ["to", "cc"])
        XCTAssertEqual(MessageHeaderMode.all.recipientKinds, ["to", "cc", "bcc", "reply-to"])
        XCTAssertFalse(MessageHeaderMode.extended.showsTechnicalHeaders)
        XCTAssertTrue(MessageHeaderMode.all.showsTechnicalHeaders)
    }

    func testDefaultKeepsTodaysReader() {
        XCTAssertEqual(MessageHeaderMode.defaultMode, .extended)
    }

    @MainActor
    func testModePersistsInPreferences() {
        let suite = "castellum-header-mode-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let first = Preferences(defaults: defaults)
        XCTAssertEqual(first.headerMode, .extended)
        first.headerMode = .compact
        XCTAssertEqual(Preferences(defaults: defaults).headerMode, .compact)
        defaults.set("garbage", forKey: "messageHeaderMode")
        XCTAssertEqual(Preferences(defaults: defaults).headerMode, .extended)
    }

    func testCommandsAreBoundToOptionCommandDigits() {
        for (id, key) in [(CommandID.headerCompact, "1"), (.headerExtended, "2"), (.headerAll, "3")] {
            let spec = CommandRegistry.spec(for: id)
            XCTAssertEqual(spec.menuChord?.key, key)
            XCTAssertEqual(spec.menuChord?.command, true)
            XCTAssertEqual(spec.menuChord?.option, true)
            XCTAssertEqual(spec.menu?.name, .view)
        }
    }

    func testRawHeaderBlockStopsAtTheBlankLine() {
        let raw = "From: a@x\r\nSubject: hi\r\n  continued\r\nX-Spam: no\r\n\r\nBody line\r\nMore"
        XCTAssertEqual(
            RawHeaderBlock.extract(from: raw),
            "From: a@x\nSubject: hi\n  continued\nX-Spam: no")
    }

    func testRawHeaderBlockIsTotal() {
        XCTAssertEqual(RawHeaderBlock.extract(from: ""), "")
        XCTAssertEqual(RawHeaderBlock.extract(from: "\n\nbody"), "")
        let runaway = (0..<5_000).map { "H\($0): v" }.joined(separator: "\n")
        XCTAssertEqual(
            RawHeaderBlock.extract(from: runaway).split(separator: "\n").count,
            RawHeaderBlock.maxLines)
    }

    // MARK: - Print follows the header mode (#2703)

    private func printDetail() -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb",
                subject: "Plans", fromName: "Ada Lovelace", fromAddress: "ada@x.test",
                sentAt: 1_792_139_400, snippet: nil, hasAttachments: false, hasBody: true,
                isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: "<orig@x.test>",
            addresses: [
                Address(kind: "from", name: "Ada Lovelace", address: "ada@x.test"),
                Address(kind: "to", name: "Bob <&> Co", address: "bob@x.test"),
                Address(kind: "cc", name: nil, address: "carol@x.test"),
                Address(kind: "bcc", name: nil, address: "dave@x.test"),
                Address(kind: "reply-to", name: nil, address: "list@x.test"),
            ],
            textBody: "See you then.", sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
    }

    @MainActor
    func testPrintCompactShowsOnlySenderDateAndSubject() {
        let html = MessagePrint.printableHTML(detail: printDetail(), mode: .compact)
        XCTAssertTrue(html.contains("<th>From</th>"))
        XCTAssertTrue(html.contains("<th>Subject</th>"))
        for label in ["To", "Cc", "Bcc", "Reply-To", "Message-ID"] {
            XCTAssertFalse(html.contains("<th>\(label)</th>"), label)
        }
    }

    @MainActor
    func testPrintExtendedShowsToAndCcButNotBcc() {
        let html = MessagePrint.printableHTML(detail: printDetail(), mode: .extended)
        XCTAssertTrue(html.contains("<th>To</th>"))
        XCTAssertTrue(html.contains("<th>Cc</th><td>carol@x.test</td>"))
        XCTAssertFalse(html.contains("dave@x.test"), "Bcc must not print in extended mode")
        XCTAssertFalse(html.contains("<th>Message-ID</th>"))
        // The default (no mode passed) is the reader's default, extended.
        XCTAssertEqual(MessagePrint.printableHTML(detail: printDetail()), html)
    }

    @MainActor
    func testPrintAllShowsBccReplyToAndMessageIdEscaped() {
        let html = MessagePrint.printableHTML(detail: printDetail(), mode: .all)
        XCTAssertTrue(html.contains("<th>Bcc</th><td>dave@x.test</td>"))
        XCTAssertTrue(html.contains("<th>Reply-To</th><td>list@x.test</td>"))
        XCTAssertTrue(html.contains("<th>Message-ID</th><td>&lt;orig@x.test&gt;</td>"))
        XCTAssertTrue(html.contains("Bob &lt;&amp;&gt; Co"), "names are escaped")
        XCTAssertFalse(html.contains("Bob <&> Co"))
    }
}
