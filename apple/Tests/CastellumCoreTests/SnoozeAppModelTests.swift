// Castellum - Tests for S43's snooze selection state on AppModel.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class SnoozeSelectionAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-snooze-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func mailbox(_ id: String, accountId: String, role: String) -> Mailbox {
        Mailbox(id: id, accountId: accountId, path: "INBOX", name: "INBOX", role: role,
                delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    func testSelectingSnoozedClearsMailboxAndUnifiedSelection() {
        let model = makeModel()
        model.select(mailbox: mailbox("a:INBOX", accountId: "a", role: "inbox"))
        model.select(snoozed: ())
        XCTAssertTrue(model.selectedIsSnoozed)
        XCTAssertNil(model.selectedMailbox)
        XCTAssertNil(model.selectedUnifiedRole)
    }

    private func sentinel() -> MessageHeader {
        MessageHeader(
            id: "s1", accountId: "a", mailboxId: "a:INBOX",
            subject: "search hit", fromName: nil, fromAddress: "x@y.test",
            sentAt: 1, snippet: nil, hasAttachments: false, hasBody: true,
            isRead: true, isStarred: false, threadCount: 1)
    }

    /// Audit #3441: the snooze sweep/refresh must not overwrite search results
    /// typed inside the Snoozed view; without a search it still refreshes the list.
    func testSnoozedRefreshDoesNotOverwriteSearchResults() async throws {
        let model = makeModel()
        model.select(snoozed: ())
        model.messages = [sentinel()]
        model.searchText = "hit"
        model.reloadSnoozedMessages()
        try await Task.sleep(nanoseconds: 400_000_000)
        XCTAssertEqual(model.messages.map(\.id), ["s1"], "search results must survive the refresh")

        model.searchText = ""
        model.reloadSnoozedMessages()
        try await Task.sleep(nanoseconds: 400_000_000)
        XCTAssertTrue(model.messages.isEmpty, "with no search, the snoozed list (empty here) replaces the rows")
    }

    func testSelectingAMailboxClearsTheSnoozedSelection() {
        let model = makeModel()
        model.select(snoozed: ())
        model.select(mailbox: mailbox("a:INBOX", accountId: "a", role: "inbox"))
        XCTAssertFalse(model.selectedIsSnoozed)
    }
}

/// A real account synced against GreenMail; snoozing a message must hide it
/// from the mailbox list, and the sweep must bring it back once due. Skips —
/// not fails — when GreenMail is unreachable.
@MainActor
final class SnoozeGreenMailAppModelTests: XCTestCase {
    private static let host = "127.0.0.1"
    private static let port: UInt16 = 3143
    private static let smtpPort: UInt16 = 3025

    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-snooze-e2e-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func greenMailReachable() -> Bool {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = Self.port.bigEndian
        addr.sin_addr.s_addr = inet_addr(Self.host)
        return withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
    }

    private func config(accountId: String, email: String) -> String {
        """
        {"account_id":"\(accountId)","email":"\(email)","host":"\(Self.host)",
         "port":\(Self.port),"username":"\(email)","password":"\(email)",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
    }

    private struct SMTPError: Error { let reason: String }

    /// Deliver one bootstrap message via raw SMTP so GreenMail auto-creates
    /// `to` (login == password == the address) and the inbox has something to
    /// snooze. IMAP login alone does NOT auto-create an account (CLAUDE.md
    /// gotcha 4), so this step is required before `addAccount`'s IMAP sync —
    /// copied verbatim from `UnifiedInboxGreenMailAppModelTests` (S34).
    private func provision(to: String) throws {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = Self.smtpPort.bigEndian
        addr.sin_addr.s_addr = inet_addr(Self.host)
        let connected = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
        guard connected else { throw SMTPError(reason: "connect GreenMail SMTP") }

        var pending: [UInt8] = []
        func readLine() throws -> String {
            while true {
                if let idx = pending.firstIndex(of: 0x0A) {
                    let line = String(decoding: pending[...idx], as: UTF8.self)
                    pending.removeSubrange(...idx)
                    return line
                }
                var chunk = [UInt8](repeating: 0, count: 512)
                let n = recv(sock, &chunk, chunk.count, 0)
                guard n > 0 else { throw SMTPError(reason: "SMTP connection closed early") }
                pending.append(contentsOf: chunk[0..<n])
            }
        }
        func expect(_ code: String) throws {
            while true {
                let line = try readLine()
                guard line.hasPrefix(code) else { throw SMTPError(reason: "expected \(code), got: \(line)") }
                let bytes = Array(line.utf8)
                if bytes.count > 3, bytes[3] == 0x20 { break }
            }
        }
        func send(_ s: String) throws {
            let bytes = Array(s.utf8)
            let sent = bytes.withUnsafeBufferPointer { write(sock, $0.baseAddress, $0.count) }
            guard sent == bytes.count else { throw SMTPError(reason: "short SMTP write") }
        }

        try expect("220")
        try send("HELO castellum-e2e\r\n")
        try expect("250")
        try send("MAIL FROM:<bootstrap@local.test>\r\n")
        try expect("250")
        try send("RCPT TO:<\(to)>\r\n")
        try expect("250")
        try send("DATA\r\n")
        try expect("354")
        try send("Subject: castellum e2e account bootstrap\r\n\r\nbootstrap for \(to)\r\n\r\n.\r\n")
        try expect("250")
        try send("QUIT\r\n")
        try expect("221")
    }

    private func waitUntil(
        // 45s, not 15s: these two tests do a real IMAP sync against
        // GreenMail, and on the LAN CI runner the same Mac is often
        // building three other worktrees at once. Both failed there on
        //
        // The timeout was raised from 15s on the theory that a deadline was
        // what ran out. That was only half the story, and the wrong half:
        // the failing runs died on `model.mailboxes[id]!` — a force unwrap of
        // a list that arrives on a LATER async hop than the `addAccount`
        // completion the test waited on. Under load that hop had not landed,
        // the unwrap trapped, and a trap takes the whole test PROCESS down —
        // which is why the sibling test was reported as failing too without
        // ever having run. Waiting for the mailbox list explicitly (see
        // `selectInbox`) fixes the race; the generous timeout stays, because
        // the sync really is slow on a loaded runner.
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 45,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    /// Selects the new account's inbox once the mailbox list has actually
    /// arrived. Never force-unwraps: an XCTUnwrap failure fails THIS test,
    /// whereas a trap aborts the process and takes every other test with it.
    private func selectInbox(_ model: AppModel, _ id: String) async throws {
        try await waitUntil(
            { model.mailboxes[id]?.contains { $0.role == "inbox" } == true },
            "the mailbox list for the new account never arrived")
        let inbox = try XCTUnwrap(
            model.mailboxes[id]?.first { $0.role == "inbox" },
            "no inbox-role mailbox for the new account")
        model.select(mailbox: inbox)
    }

    func testSnoozingHidesAMessageAndTheSweepReturnsItOnceDue() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let id = "snooze-e2e-\(UUID().uuidString)"
        let email = "e2e-\(id)@local.test"

        // GreenMail auto-creates an account only on SMTP delivery (IMAP login
        // alone does not — CLAUDE.md gotcha 4); this also seeds one message
        // for the inbox to show.
        try provision(to: email)

        var addResult: AccountSyncStatus?
        model.addAccount(
            accountID: id, configJSON: config(accountId: id, email: email),
            completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "account never finished syncing")
        try await selectInbox(model, id)
        try await waitUntil({ !model.messages.isEmpty }, "inbox never populated")

        let target = model.messages[0]
        model.selectedMessageIds = [target.id]
        // 5 s, not 1 s: on a loaded CI runner (apple-build on 1d36c0bf, load
        // ~60) the snooze write + list reload took longer than a 1 s window, so
        // the message was already due (and could be swept back) before the
        // test ever saw it leave the inbox.
        let snoozeSeconds: TimeInterval = 5
        model.snoozeSelected(until: Int64(Date().addingTimeInterval(snoozeSeconds).timeIntervalSince1970))
        try await waitUntil({ !model.messages.contains { $0.id == target.id } }, "message never left the inbox")

        // Wait past the snooze time, then run the sweep directly (avoids a
        // real 30s wait for the background loop).
        try await Task.sleep(nanoseconds: UInt64((snoozeSeconds + 0.5) * 1_000_000_000))
        model.startSnoozeSweep() // re-run immediately rather than wait for the loop's next tick
        try await waitUntil({ model.messages.contains { $0.id == target.id } }, "message never returned once due")
    }

    /// Regression for the review round that found `select(snoozed:)` never
    /// actually populated `messages` (MessageListView renders only that
    /// array, never `snoozedMessages` directly) and `unsnoozeSelected()` had
    /// no caller anywhere in the app: snooze a message, select the Snoozed
    /// sidebar row, confirm the message is actually LISTED there (not just
    /// counted in the badge), then unsnooze it from that list and confirm it
    /// leaves Snoozed and is back in the Inbox — without waiting for the due
    /// sweep at all.
    func testSnoozedListRendersTheMessageAndUnsnoozeSelectedReturnsItToTheInbox() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let id = "unsnooze-e2e-\(UUID().uuidString)"
        let email = "e2e-\(id)@local.test"

        try provision(to: email)

        var addResult: AccountSyncStatus?
        model.addAccount(
            accountID: id, configJSON: config(accountId: id, email: email),
            completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "account never finished syncing")
        try await selectInbox(model, id)
        try await waitUntil({ !model.messages.isEmpty }, "inbox never populated")

        let target = model.messages[0]
        model.selectedMessageIds = [target.id]
        // Far enough out that the background sweep cannot race this test.
        model.snoozeSelected(until: Int64(Date().addingTimeInterval(3600).timeIntervalSince1970))
        try await waitUntil({ !model.messages.contains { $0.id == target.id } }, "message never left the inbox")

        model.select(snoozed: ())
        // A generous timeout: this is the fourth account/list round-trip in
        // this test alone, and under a full-suite run GreenMail and the
        // shared Swift-concurrency thread pool are also carrying every other
        // GreenMail-gated test's traffic.
        try await waitUntil(
            { model.messages.contains { $0.id == target.id } }, timeout: 30,
            "Snoozed row selected but the snoozed message never appeared in `messages` " +
                "— MessageListView only ever renders `messages`")
        XCTAssertTrue(
            model.snoozedMessages.contains { $0.id == target.id },
            "snoozedMessages (the badge count's source) should also list it")

        model.selectedMessageIds = [target.id]
        model.unsnoozeSelected()
        try await waitUntil(
            { !model.messages.contains { $0.id == target.id } }, timeout: 30,
            "unsnoozeSelected() never removed the message from the Snoozed list")
        XCTAssertFalse(model.snoozedMessages.contains { $0.id == target.id })

        try await selectInbox(model, id)
        try await waitUntil(
            { model.messages.contains { $0.id == target.id } }, timeout: 30,
            "unsnoozeSelected() never returned the message to the Inbox")
    }
}
