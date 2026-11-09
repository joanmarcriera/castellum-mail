// Castellum - Tests for S34's unified-selection AppModel state and the
// compose-default-account priority it changes.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class UnifiedInboxAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-unified-\(UUID().uuidString)")
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

    private func account(_ id: String) -> Account {
        Account(id: id, email: "\(id)@local.test", displayName: nil, provider: "imap")
    }

    private func mailbox(_ id: String, accountId: String, role: String) -> Mailbox {
        Mailbox(id: id, accountId: accountId, path: "INBOX", name: "INBOX", role: role,
                delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    private func header(_ id: String, accountId: String) -> MessageHeader {
        MessageHeader(id: id, accountId: accountId, mailboxId: "\(accountId):INBOX",
                      subject: nil, fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
    }

    func testIsUnifiedListTracksSelectedUnifiedRole() {
        let model = makeModel()
        XCTAssertFalse(model.isUnifiedList)
        model.selectedUnifiedRole = "inbox"
        XCTAssertTrue(model.isUnifiedList)
    }

    func testSelectingAMailboxClearsTheUnifiedSelectionAndViceVersa() {
        let model = makeModel()
        model.selectedUnifiedRole = "inbox"
        model.select(mailbox: mailbox("a:INBOX", accountId: "a", role: "inbox"))
        XCTAssertNil(model.selectedUnifiedRole)
        XCTAssertEqual(model.selectedMailbox?.id, "a:INBOX")
    }

    func testComposeDefaultPrefersTheSelectedMessagesAccountOverTheMailboxsAccount() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.selectedMailbox = mailbox("a:INBOX", accountId: "a", role: "inbox")
        model.selectedMessage = header("b:INBOX:1", accountId: "b")
        XCTAssertEqual(model.composeDefaultAccountId, "b", "a unified view's selected message wins")
    }

    func testComposeDefaultFallsBackToTheSelectedMailboxThenTheFirstAccount() {
        let model = makeModel()
        model.accounts = [account("a")]
        XCTAssertEqual(model.composeDefaultAccountId, "a")
        model.selectedMailbox = mailbox("a:INBOX", accountId: "a", role: "inbox")
        XCTAssertEqual(model.composeDefaultAccountId, "a")
    }

    /// #3459: a g-chord in All Inboxes goes to the SELECTED MESSAGE's account, not the first.
    func testGChordRoleJumpUsesTheSelectedMessagesAccount() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.mailboxes = [
            "a": [mailbox("a:Archive", accountId: "a", role: "archive")],
            "b": [mailbox("b:Archive", accountId: "b", role: "archive")],
        ]
        model.selectedUnifiedRole = "inbox"
        model.selectedMessage = header("b:INBOX:1", accountId: "b")
        model.selectMailbox(role: "archive")
        XCTAssertEqual(model.selectedMailbox?.id, "b:Archive")
    }

    func testGChordRoleJumpFallsBackToTheFirstAccountWithNothingSelected() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.mailboxes = [
            "a": [mailbox("a:Archive", accountId: "a", role: "archive")],
            "b": [mailbox("b:Archive", accountId: "b", role: "archive")],
        ]
        model.selectedUnifiedRole = "inbox"
        model.selectMailbox(role: "archive")
        XCTAssertEqual(model.selectedMailbox?.id, "a:Archive")
    }

    func testEmptyTrashNeverFallsBackToTheFirstAccountInAUnifiedList() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.selectedUnifiedRole = "inbox"
        XCTAssertNil(model.emptyTrashAccountId, "no mailbox selected: must not guess accounts.first")
        model.emptyTrash()
        XCTAssertNotNil(model.errorText, "the refusal must be visible, not silent")
    }

    func testEmptyTrashTargetsTheSelectedMailboxsAccount() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.selectedMailbox = mailbox("b:INBOX", accountId: "b", role: "inbox")
        XCTAssertEqual(model.emptyTrashAccountId, "b")
    }

    /// FP22: with no mailbox (and no account) there is no Trash to empty; the
    /// refusal must still be visible, not a silent no-op.
    func testEmptyTrashWithNoMailboxSelectedExplainsItself() {
        let model = makeModel()
        XCTAssertNil(model.emptyTrashAccountId)
        model.emptyTrash()
        XCTAssertEqual(model.errorText, String(localized: "There is no Trash to empty. Select a mailbox first."))
    }

    /// FP22: the irreversible-erase confirmation names whose Trash it erases.
    func testEmptyTrashConfirmationTitleNamesTheAccount() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.selectedMailbox = mailbox("b:INBOX", accountId: "b", role: "inbox")
        XCTAssertTrue(model.emptyTrashConfirmationTitle.contains("b@local.test"),
                      model.emptyTrashConfirmationTitle)
        XCTAssertFalse(model.emptyTrashConfirmationTitle.contains("a@local.test"))
    }
}

/// Two real accounts synced against GreenMail; `AppModel.select(unifiedRole:)`
/// must populate `messages` from both. Requires local GreenMail (see
/// `scripts/local-imap`); skips — not fails — when it's unreachable.
@MainActor
final class UnifiedInboxGreenMailAppModelTests: XCTestCase {
    private static let host = "127.0.0.1"
    private static let port: UInt16 = 3143
    private static let smtpPort: UInt16 = 3025

    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-unified-e2e-\(UUID().uuidString)")
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
    /// `to` (login == password == the address — verified against GreenMail
    /// 2.1.9, same convention as `castellum-core`'s Rust `common::provision`)
    /// and the unified inbox has something to show. IMAP login alone does
    /// NOT auto-create an account (CLAUDE.md gotcha 4), so this step is
    /// required before `addAccount`'s IMAP sync, not merely a content seed.
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
        // Multi-line replies repeat the code with '-' until the final
        // "<code> " line; read until that terminator (mirrors common::smtp_deliver).
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
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 15,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    func testSelectUnifiedRoleShowsMessagesFromBothAccounts() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let idA = "unified-a-\(UUID().uuidString)"
        let idB = "unified-b-\(UUID().uuidString)"
        let emailA = "e2e-\(idA)@local.test"
        let emailB = "e2e-\(idB)@local.test"

        // GreenMail auto-creates an account only on SMTP delivery (IMAP login
        // alone does not — CLAUDE.md gotcha 4); this also seeds one message
        // per account for the unified list to show.
        try provision(to: emailA)
        try provision(to: emailB)

        var addResults: [String: AccountSyncStatus] = [:]
        model.addAccount(
            accountID: idA, configJSON: config(accountId: idA, email: emailA),
            completion: { addResults[idA] = $0 })
        try await waitUntil({ addResults[idA] != nil }, "account A never finished syncing")
        model.addAccount(
            accountID: idB, configJSON: config(accountId: idB, email: emailB),
            completion: { addResults[idB] = $0 })
        try await waitUntil({ addResults[idB] != nil }, "account B never finished syncing")

        model.select(unifiedRole: "inbox")
        // FP02: adding account A already selected the unified inbox, so the list
        // is non-empty (A's mail only) before this select lands; wait for both.
        try await waitUntil({ Set(model.messages.map(\.accountId)).isSuperset(of: [idA, idB]) },
                            "unified inbox never showed both accounts")

        let seenAccounts = Set(model.messages.map(\.accountId))
        XCTAssertTrue(seenAccounts.contains(idA) && seenAccounts.contains(idB),
                       "expected both accounts, saw \(seenAccounts)")
    }
}

/// #3459: Archive must not drop rows that already sit in an Archive mailbox.
final class ArchiveAlreadyArchivedTests: XCTestCase {
    private func row(_ id: String, mailbox: String) -> MessageHeader {
        MessageHeader(id: id, accountId: "a", mailboxId: mailbox,
                      subject: nil, fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
    }

    func testRowsInAnArchiveMailboxAreSkipped() {
        let boxes = ["a": [
            Mailbox(id: "a:INBOX", accountId: "a", path: "INBOX", name: "INBOX", role: "inbox",
                    delimiter: "/", subscribed: true, selectable: true, syncByDefault: true),
            Mailbox(id: "a:Archive", accountId: "a", path: "Archive", name: "Archive", role: "archive",
                    delimiter: "/", subscribed: true, selectable: true, syncByDefault: true),
        ]]
        let rows = [row("m1", mailbox: "a:INBOX"), row("m2", mailbox: "a:Archive")]
        XCTAssertEqual(
            AppModel.idsNotAlreadyArchived(["m1", "m2"], rows: rows, mailboxes: boxes, fallbackRole: nil),
            ["m1"])
        XCTAssertEqual(
            AppModel.idsNotAlreadyArchived(["m3"], rows: rows, mailboxes: boxes, fallbackRole: nil),
            ["m3"], "an unknown id is passed through for the core to judge")
    }
}
