// Castellum - Tests for S35's reset-account refusal classifier and AppModel
// wiring.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AccountResetPolicyTests: XCTestCase {
    func testMatchesTheStablePrefix() {
        XCTAssertTrue(AccountResetPolicy.isResetBlockedRefusal(
            CastellumError.message("reset blocked: 1 unsent outbox draft(s), 0 pending operation(s)")))
    }

    func testDoesNotMatchAnUnrelatedMessage() {
        XCTAssertFalse(AccountResetPolicy.isResetBlockedRefusal(CastellumError.message("unknown account id: x")))
        XCTAssertFalse(AccountResetPolicy.isResetBlockedRefusal(CastellumError.io))
    }
}

@MainActor
final class ResetAccountAppModelWiringTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-reset-\(UUID().uuidString)")
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

    /// No stored credentials at all: `resetAccount` must not even attempt
    /// the FFI call — it should fail fast with `errorText`, never crash on a
    /// missing config.
    func testWithNoStoredCredentialsSetsErrorTextAndNeverCallsTheCore() {
        let model = makeModel()
        XCTAssertNil(model.errorText, "sanity")
        model.resetAccount(accountId: "no-such-account")
        XCTAssertNotNil(model.errorText)
        XCTAssertNil(model.resetAccountBlocked)
    }
}

/// Full arc through AppModel against real GreenMail: an unsent outbox draft
/// blocks the reset with `resetAccountBlocked` set; forcing discards it and
/// a fresh sync repopulates. Requires local GreenMail (see
/// `scripts/local-imap`); skips — not fails — when it's unreachable.
@MainActor
final class ResetAccountGreenMailAppModelTests: XCTestCase {
    private static let host = "127.0.0.1"
    private static let port: UInt16 = 3143
    private static let smtpPort: UInt16 = 3025

    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-reset-e2e-\(UUID().uuidString)")
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

    func testUnsentOutboxDraftBlocksThenForceResetsAndResyncs() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let accountId = "reset-e2e-\(UUID().uuidString)"
        let email = "e2e-\(accountId)@local.test"

        // GreenMail auto-creates an account only on SMTP delivery (gotcha 4).
        try provision(to: email)

        var addResult: AccountSyncStatus?
        model.addAccount(accountID: accountId, configJSON: config(accountId: accountId, email: email),
                          completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "initial add never completed")
        XCTAssertEqual(addResult?.state, .ok)
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: true)

        // A long undo-send window: this test never lets it deliver.
        let draft = NewDraft(accountId: accountId,
                              to: [DraftAddress(name: nil, email: email)],
                              subject: "still sending", bodyText: "not delivered yet")
        var draftId: Int64?
        model.saveDraft(draft) { draftId = $0 }
        try await waitUntil({ draftId != nil }, "draft save never completed")
        try model.core.sendDraft(id: draftId!, delaySeconds: 3600)

        model.resetAccount(accountId: accountId)
        try await waitUntil({ model.resetAccountBlocked != nil }, "unforced reset never surfaced the blocked prompt")
        XCTAssertEqual(model.resetAccountBlocked?.blockers.unsentOutboxDrafts, 1)

        // Mail exists before the reset; the forced reset must clear the
        // blocked prompt, discard the draft and then a fresh sync must
        // repopulate the Inbox from the server.
        let inboxId = "\(accountId):INBOX"
        let before = try model.core.listMessages(accountId: accountId, mailboxId: inboxId, limit: 50, offset: 0)
        XCTAssertFalse(before.isEmpty, "sanity: initial sync fetched the bootstrap mail")

        model.resetAccount(accountId: accountId, force: true)
        try await waitUntil({ model.resetAccountBlocked == nil }, "a successful reset clears the blocked prompt")
        let drafts = try model.core.listDrafts(accountId: accountId)
        XCTAssertTrue(drafts.isEmpty, "the forced reset must have discarded the queued draft")

        try await waitUntil({
            ((try? model.core.listMessages(accountId: accountId, mailboxId: inboxId, limit: 50, offset: 0)) ?? []).isEmpty == false
        }, "resync never repopulated the Inbox")
    }
}
