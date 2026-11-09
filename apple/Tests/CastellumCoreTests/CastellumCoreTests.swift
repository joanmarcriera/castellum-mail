// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

final class CastellumCoreTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-swift-test-\(UUID().uuidString)")
    }

    func testOpenReportsVersionAndSchema() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        XCTAssertFalse(core.coreVersion.isEmpty)
        XCTAssertGreaterThanOrEqual(try core.schemaVersion(), 1)
    }

    /// Draft CRUD over the real FFI: insert -> list -> update in place -> delete.
    /// (Queue + SMTP delivery need a synced account row — the `operations` table
    /// has an FK on accounts — and are covered by the Rust core's own tests.)
    func testDraftLifecycleOverFFI() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        XCTAssertEqual(try core.listDrafts(accountId: "acct").count, 0)

        var draft = NewDraft(accountId: "acct",
                             to: [DraftAddress(name: "Ada", email: "ada@x.test")],
                             subject: "Hi", bodyText: "First")
        let id = try core.saveDraft(draft)
        XCTAssertGreaterThan(id, 0)

        var listed = try core.listDrafts(accountId: "acct")
        XCTAssertEqual(listed.count, 1)
        XCTAssertEqual(listed[0].subject, "Hi")
        XCTAssertEqual(listed[0].to, draft.to)
        XCTAssertEqual(listed[0].status, "draft")

        // Update in place (same id, no duplicate row).
        draft.id = id
        draft.bodyText = "Second"
        XCTAssertEqual(try core.saveDraft(draft), id)
        listed = try core.listDrafts(accountId: "acct")
        XCTAssertEqual(listed.count, 1)
        XCTAssertEqual(listed[0].bodyText, "Second")

        // Queuing a draft for an account that doesn't exist is refused (FK):
        // sends only work against a real synced account.
        XCTAssertThrowsError(try core.sendDraft(id: id))

        // With nothing queued, a send pass reports 0/0 (SendReport over FFI).
        let cfg = AccountConfig(accountId: "acct", email: "me@x.test",
                                host: "imap.x.test", port: 993,
                                username: "me@x.test", password: "pw",
                                useTls: false, allowInvalidCerts: nil,
                                smtpHost: "127.0.0.1", smtpPort: 1,
                                smtpUsername: "me@x.test", smtpPassword: "pw",
                                smtpUseTls: false)
        let report = try core.sendPending(configJSON: cfg.sendConfigJSON())
        XCTAssertEqual(report, SendReport(accountId: "acct", sent: 0, failed: 0))
    }

    func testDeleteDraftRemovesIt() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let id = try core.saveDraft(NewDraft(accountId: "acct", to: []))
        XCTAssertEqual(try core.listDrafts(accountId: "acct").count, 1)
        try core.deleteDraft(id: id)
        XCTAssertEqual(try core.listDrafts(accountId: "acct").count, 0)
    }

    func testEmptyProfileQueriesReturnEmpty() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        XCTAssertEqual(try core.listAccounts().count, 0)
        XCTAssertNil(try core.getMessage(id: "nope"))
        XCTAssertEqual(try core.search(accountId: "a", query: "x", limit: 10).count, 0)
        XCTAssertEqual(try core.listThreads(accountId: "a", limit: 10, offset: 0).count, 0)
        XCTAssertEqual(try core.listMailboxes(accountId: "a").count, 0)
        XCTAssertEqual(try core.mailboxCounts(accountId: "a").count, 0)
        XCTAssertEqual(try core.pendingOperations(accountId: "a").count, 0)
        XCTAssertThrowsError(try core.markRead(messageId: "nope", isRead: true))
    }

    /// S58: the CoreSpotlight indexing opt-in defaults OFF and round-trips
    /// over the real FFI/`app_meta` — the same profile-durable persistence
    /// `account_write_enabled` uses, proved directly against the Rust core.
    func testSpotlightIndexingEnabledDefaultsOffAndRoundTrips() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        XCTAssertFalse(try core.spotlightIndexingEnabled(), "a fresh profile must default OFF")

        try core.setSpotlightIndexingEnabled(true)
        XCTAssertTrue(try core.spotlightIndexingEnabled())

        try core.setSpotlightIndexingEnabled(false)
        XCTAssertFalse(try core.spotlightIndexingEnabled())
    }
}
