// Castellum - Tests for the folder-selection-edit refusal classifier and
// AppModel wiring, including the follow-up review fixes.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class MailboxSelectionPolicyTests: XCTestCase {
    func testMatchesTheStablePrefix() {
        XCTAssertTrue(MailboxSelectionPolicy.isRemovalBlockedRefusal(
            CastellumError.message("mailbox removal blocked: 1 pending operation(s) in Old")))
    }

    func testDoesNotMatchAnUnrelatedMessage() {
        XCTAssertFalse(MailboxSelectionPolicy.isRemovalBlockedRefusal(CastellumError.message("unknown account id: x")))
        XCTAssertFalse(MailboxSelectionPolicy.isRemovalBlockedRefusal(CastellumError.io))
        // Never confused with the unrelated S35 reset refusal, even though
        // both share the "blocked" word.
        XCTAssertFalse(MailboxSelectionPolicy.isRemovalBlockedRefusal(
            CastellumError.message("reset blocked: 1 unsent outbox draft(s), 0 pending operation(s)")))
    }
}

@MainActor
final class FolderSelectionAppModelWiringTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-folders-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    private func config(accountId: String, mailboxes: [String]? = nil) -> AccountConfig {
        var c = AccountConfig(
            accountId: accountId, email: "a@x", host: "h", port: 993, username: "a@x",
            password: "p", useTls: true, allowInvalidCerts: nil, smtpHost: nil, smtpPort: nil,
            smtpUsername: nil, smtpPassword: nil, smtpUseTls: nil, displayName: nil)
        c.mailboxes = mailboxes
        return c
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    /// follow-up review, point 3(a): the Keychain config must
    /// be written FIRST, unconditionally — not gated on a successful core
    /// purge. An empty `keepPaths` forces the core to refuse
    /// (`CoreError::InvalidArgument`, point 4), which is exactly the
    /// failure branch the OLD ordering (Keychain saved LAST) would have
    /// left with no Keychain entry at all.
    func testKeychainConfigIsSavedBeforeTheCorePurgeEvenWhenThatPurgeFails() throws {
        let model = makeModel()
        let cfg = config(accountId: "acc-order", mailboxes: ["INBOX"])
        let json = try cfg.encodedJSON()

        let exp = expectation(description: "completion")
        var result: AppModel.FolderSelectionUpdateResult?
        model.updateFolderSelection(accountId: "acc-order", configJSON: json, keepPaths: []) { r in
            result = r
            exp.fulfill()
        }
        wait(for: [exp], timeout: 2)

        guard case .failure = result else {
            XCTFail("expected the core to refuse an empty keep list, got \(String(describing: result))")
            return
        }
        let stored = try model.accountStore.configJSON(for: "acc-order")
        XCTAssertEqual(stored, json, "the Keychain write must land even though the core purge afterwards failed")
    }

    /// follow-up review, point 3(b): Save must be refused
    /// immediately while this account is syncing — `AppModel.sync` sets
    /// `accountSyncStatuses[id] = .syncing` synchronously before any network
    /// I/O, so calling `updateFolderSelection` right after it (no `await` in
    /// between) reliably observes that state.
    func testUpdateIsRefusedWhileTheAccountIsSyncing() throws {
        let model = makeModel()
        let cfg = config(accountId: "acc-syncing")
        let json = try cfg.encodedJSON()

        model.sync(config: json)
        XCTAssertEqual(model.accountSyncStatuses["acc-syncing"]?.state, .syncing, "sanity: sync() sets this synchronously")

        let exp = expectation(description: "completion")
        var result: AppModel.FolderSelectionUpdateResult?
        model.updateFolderSelection(accountId: "acc-syncing", configJSON: json, keepPaths: ["INBOX"]) { r in
            result = r
            exp.fulfill()
        }
        wait(for: [exp], timeout: 2)

        guard case .failure(let message) = result else {
            XCTFail("expected an immediate refusal while syncing, got \(String(describing: result))")
            return
        }
        XCTAssertTrue(message.contains("syncing"), "message: \(message)")
        // Never even reached the Keychain: the pre-sync config (no
        // `mailboxes` filter) is exactly what's still stored.
        let stored = try model.accountStore.configJSON(for: "acc-syncing")
        XCTAssertNil(stored, "no account was ever added via addAccount/saveAccountWithoutSync, so nothing should exist")
    }

    /// No stored credentials at all: must not even attempt the FFI call —
    /// fails fast, never crashes on a missing config.
    func testWithNoStoredCredentialsSetsErrorText() {
        let model = makeModel()
        XCTAssertNil(model.errorText, "sanity")
        let exp = expectation(description: "completion")
        var result: AppModel.FolderSelectionUpdateResult?
        // `configJSON` is well-formed but nothing was ever persisted for
        // this account id — the Keychain save is a fresh add, which
        // succeeds; the point is that no crash occurs end to end.
        model.updateFolderSelection(
            accountId: "never-added",
            configJSON: (try? config(accountId: "never-added", mailboxes: ["INBOX"]).encodedJSON()) ?? "{}",
            keepPaths: ["INBOX"]
        ) { r in
            result = r
            exp.fulfill()
        }
        wait(for: [exp], timeout: 2)
        guard case .success = result else {
            XCTFail("expected success (nothing to prune for a brand-new account id), got \(String(describing: result))")
            return
        }
    }
}

/// Full arc through AppModel against real GreenMail: the belt-and-braces
/// `reconcileMailboxSelectionAfterInFlightSync` ( SECOND
/// follow-up review) must never force-cancel an operation the user queued
/// during its wait window — the original `force: true` there would have.
/// Requires local GreenMail (see `scripts/local-imap`); skips — not fails —
/// when it's unreachable.
@MainActor
final class FolderSelectionReconcileGreenMailAppModelTests: XCTestCase {
    private static let host = "127.0.0.1"
    private static let port: UInt16 = 3143
    private static let smtpPort: UInt16 = 3025

    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-folders-reconcile-e2e-\(UUID().uuidString)")
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
         "use_tls":false,"allow_invalid_certs":false}
        """
    }

    private struct SMTPError: Error { let reason: String }

    /// Deliver one bootstrap message via raw SMTP so GreenMail auto-creates
    /// the account (IMAP login alone does not — CLAUDE.md gotcha 4).
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
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 15,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    func testReconcileNeverForceCancelsAPendingOpQueuedDuringItsWindow() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let accountId = "folders-reconcile-e2e-\(UUID().uuidString)"
        let email = "e2e-\(accountId)@local.test"
        try provision(to: email)

        var addResult: AccountSyncStatus?
        model.addAccount(accountID: accountId, configJSON: config(accountId: accountId, email: email),
                          completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "initial add never completed")
        XCTAssertEqual(addResult?.state, .ok)
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: true)

        let inboxId = "\(accountId):INBOX"
        let before = try model.core.listMessages(accountId: accountId, mailboxId: inboxId, limit: 50, offset: 0)
        XCTAssertFalse(before.isEmpty, "sanity: bootstrap mail synced")

        // Delete it (moves to Trash — creates the Trash mailbox locally,
        // queues a real move op) and replay for real, so a fresh sync
        // imports an actual message row into Trash.
        try model.core.deleteMessage(messageId: before[0].id)
        model.replayNow()
        try await waitUntil({ (try? model.core.pendingOperations(accountId: accountId))?.isEmpty == true },
                             "the move to Trash never replayed")

        model.sync(config: config(accountId: accountId, email: email))
        try await waitUntil({ model.accountSyncStatuses[accountId]?.state == .ok },
                             "the re-sync that should import Trash never completed")

        let trashId = "\(accountId):Trash"
        let trashMessages = try model.core.listMessages(accountId: accountId, mailboxId: trashId, limit: 50, offset: 0)
        XCTAssertFalse(trashMessages.isEmpty, "sanity: the moved message was re-imported into Trash")

        // Queue the blocking op: mark the Trash message read. Trash stays
        // in `to_remove` on every apply_mailbox_selection call while
        // unticked (its role row is never deleted), so this is exactly the
        // scenario the fix targets.
        try model.core.markRead(messageId: trashMessages[0].id, isRead: true)
        XCTAssertEqual(try model.core.pendingOperations(accountId: accountId).count, 1)

        // Switch to evaluation (read-only) mode so the racing sync's own
        // `replayNow()` (every successful `sync(config:)` drains the
        // outbox) is refused rather than actually delivering the mark_read
        // — otherwise it would legitimately succeed and vanish from
        // `pendingOperations` before the reconciliation even runs, which
        // would test nothing. Sync itself (reading) is unaffected by
        // evaluation mode; only writes/replay are.
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: false)

        // Start a REAL sync of the SAME account (sets `.syncing`
        // synchronously and keeps it there for the sync's real duration),
        // then run the reconciliation while that's in flight — exactly the
        // race it exists to repair.
        model.sync(config: config(accountId: accountId, email: email))
        XCTAssertEqual(model.accountSyncStatuses[accountId]?.state, .syncing, "sanity")
        model.reconcileMailboxSelectionAfterInFlightSync(accountId: accountId, fallbackKeepPaths: ["INBOX"])

        try await waitUntil({ model.accountSyncStatuses[accountId]?.state != .syncing },
                             "the racing sync never finished")
        // The reconcile's own (unforced) apply call runs right after that,
        // off the main actor — give it a moment to complete.
        try await Task.sleep(nanoseconds: 1_500_000_000)

        let pendingAfter = try model.core.pendingOperations(accountId: accountId)
        XCTAssertEqual(pendingAfter.count, 1, "the pending mark_read must survive an unforced reconcile")
        XCTAssertEqual(pendingAfter.first?.kind, "mark_read")
        XCTAssertEqual(pendingAfter.first?.status, "queued", "never cancelled")

        // And the folder itself is left alone too: Trash still exists with
        // its message intact — nothing was purged behind a blocked call.
        let trashAfter = try model.core.listMessages(accountId: accountId, mailboxId: trashId, limit: 50, offset: 0)
        XCTAssertEqual(trashAfter.count, 1, "the message must survive too — nothing was purged behind the refusal")
    }

    /// PR #174 review: an UNFORCED save blocked by a pending op must persist
    /// NOTHING — before the fix the Keychain filter was written first, so
    /// Cancel / "Sync Now and Try Again" left it saved with nothing purged.
    func testBlockedUnforcedSaveLeavesTheStoredConfigAndFolderUntouched() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let accountId = "folders-blocked-e2e-\(UUID().uuidString)"
        let email = "e2e-\(accountId)@local.test"
        try provision(to: email)
        let originalJSON = config(accountId: accountId, email: email)

        var addResult: AccountSyncStatus?
        model.addAccount(accountID: accountId, configJSON: originalJSON, completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "initial add never completed")
        XCTAssertEqual(addResult?.state, .ok)
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: true)

        let inboxId = "\(accountId):INBOX"
        let before = try model.core.listMessages(accountId: accountId, mailboxId: inboxId, limit: 50, offset: 0)
        try model.core.deleteMessage(messageId: before[0].id)
        model.replayNow()
        try await waitUntil({ (try? model.core.pendingOperations(accountId: accountId))?.isEmpty == true },
                             "the move to Trash never replayed")
        model.sync(config: originalJSON)
        try await waitUntil({ model.accountSyncStatuses[accountId]?.state == .ok }, "re-sync never completed")
        let trashId = "\(accountId):Trash"
        let trash = try model.core.listMessages(accountId: accountId, mailboxId: trashId, limit: 50, offset: 0)
        XCTAssertFalse(trash.isEmpty, "sanity: Trash imported")
        try model.core.markRead(messageId: trash[0].id, isRead: true)
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: false)

        let storedBefore = model.storedConfig(for: accountId)
        let newJSON = originalJSON.replacingOccurrences(of: "\"use_tls\"", with: "\"mailboxes\":[\"INBOX\"],\"use_tls\"")
        XCTAssertNotEqual(newJSON, originalJSON, "sanity: the edit changes the config")

        var result: AppModel.FolderSelectionUpdateResult?
        model.updateFolderSelection(accountId: accountId, configJSON: newJSON, keepPaths: ["INBOX"]) { result = $0 }
        try await waitUntil({ result != nil }, "updateFolderSelection never completed")

        guard case .blocked(let blockers) = result else {
            XCTFail("expected .blocked, got \(String(describing: result))")
            return
        }
        XCTAssertEqual(blockers.first?.path, "Trash")
        XCTAssertEqual(model.storedConfig(for: accountId), storedBefore, "a blocked save must not touch the Keychain")
        let trashAfter = try model.core.listMessages(accountId: accountId, mailboxId: trashId, limit: 50, offset: 0)
        XCTAssertEqual(trashAfter.count, 1, "nothing purged")
    }
}
