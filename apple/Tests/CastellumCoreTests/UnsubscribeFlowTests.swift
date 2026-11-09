// Castellum - S66: hardens S44's List-Unsubscribe GATES.md click script into
// a CI-provable XCTest suite. Source of truth for what each test proves is
// .run-project/GATES.md's `### S44` entries (the 8-step script) — see that
// file's own comment for the exact wording; this suite's doc comments below
// cite the step number(s) each test covers.
//
// AppModel-level testing (Assumption 4 in the S66 plan), not a new
// `XCUITest` target: the existing `UndoSendReopenTests.swift`/
// `UndoSendCrashHoldAppModelTests.swift` idiom already proves this
// codebase's convention for "does a real button action produce the real
// side effect" without literally driving pixels, and standing up a new
// UI-test target for one stage would be disproportionate to it.
//
// Test-data seam (Assumption 3): every test below constructs its message
// fixture via `DirectFixture` (direct SQLite insert — see that file's own
// header comment for why) or, where a message row isn't needed at all
// (confirming by mailto only reads `header.accountId` and
// `listUnsubscribeMailto`, never touches `messages`), a plain in-memory
// `MessageDetail` value — the same "AppModel's synthetic detail builder"
// pattern `CoreModels.swift`'s own doc comment already names as an existing
// use of `MessageDetail`'s memberwise initializer.
//
// Network seam: `OneShotMockServer` (TestSupport/) binds 127.0.0.1:0. No
// test in this file ever points a `list_unsubscribe_url`/SMTP/IMAP config
// at anything but 127.0.0.1.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

// MARK: - Shared fixture builder

/// Builds the three message fixtures the S66 plan names: (a) one-click
/// (mailto AND https, one-click flagged), (b) mailto-only, (c) no
/// List-Unsubscribe header at all. Each test gets its own account/mailbox
/// (unique per test run) so tests never interfere with each other's rows.
@MainActor
enum UnsubscribeFixtures {
    struct Seeded {
        let accountId: String
        let mailboxId: String
        let profileDirectory: URL
    }

    static func seedAccount(model: AppModel, dir: URL) throws -> Seeded {
        let accountId = "unsub-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: accountId,
            email: "test@local.test", mailboxId: mailboxId)
        return Seeded(accountId: accountId, mailboxId: mailboxId, profileDirectory: dir)
    }

    /// Fixture (a): `List-Unsubscribe: <mailto:unsub@example.com>,
    /// <https://...>` + `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
    /// — matches GATES.md step 1 verbatim. `httpsURL` should be a
    /// `OneShotMockServer`'s `urlString`.
    static func seedOneClick(_ seeded: Seeded, httpsURL: String) throws -> String {
        let id = "\(seeded.accountId):m-oneclick"
        try DirectFixture.seedMessage(
            profileDirectory: seeded.profileDirectory, id: id,
            accountId: seeded.accountId, mailboxId: seeded.mailboxId,
            subject: "Weekly newsletter", fromAddress: "news@example.com",
            listUnsubscribeMailto: "mailto:unsub@example.com",
            listUnsubscribeUrl: httpsURL, listUnsubscribeOneClick: true)
        return id
    }

    /// Fixture (b): mailto: target only, no one-click header (GATES.md step 6).
    static func seedMailtoOnly(_ seeded: Seeded) throws -> String {
        let id = "\(seeded.accountId):m-mailto"
        try DirectFixture.seedMessage(
            profileDirectory: seeded.profileDirectory, id: id,
            accountId: seeded.accountId, mailboxId: seeded.mailboxId,
            subject: "Product updates", fromAddress: "updates@example.com",
            listUnsubscribeMailto: "mailto:unsub@example.com?subject=unsubscribe",
            listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        return id
    }

    /// Fixture (c): no List-Unsubscribe header at all (GATES.md step 7).
    static func seedNoHeader(_ seeded: Seeded) throws -> String {
        let id = "\(seeded.accountId):m-plain"
        try DirectFixture.seedMessage(
            profileDirectory: seeded.profileDirectory, id: id,
            accountId: seeded.accountId, mailboxId: seeded.mailboxId,
            subject: "Hello from a friend", fromAddress: "friend@example.com",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil,
            listUnsubscribeOneClick: false)
        return id
    }
}

/// Bounded-timeout poll used throughout this file for AppModel `Task.detached`
/// work — same idiom as `UndoSendReopenTests.waitUntil`/
/// `UndoSendCrashHoldAppModelTests.waitUntil` already in this suite.
@MainActor
private func waitUntil(
    _ condition: @escaping () -> Bool, timeout: TimeInterval = 5,
    _ message: @autoclosure @escaping () -> String
) async throws {
    let deadline = Date().addingTimeInterval(timeout)
    while !condition() && Date() < deadline {
        try await Task.sleep(nanoseconds: 20_000_000)
    }
    if !condition() { XCTFail(message()) }
}

// MARK: - Task 2: link visibility (GATES.md steps 2, 7)

@MainActor
final class UnsubscribeAffordanceTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-unsub-affordance-\(UUID().uuidString)")
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

    /// Step 2: a message with a usable List-Unsubscribe target shows the
    /// affordance. Exercises the REAL `hasUnsubscribeAffordance` predicate
    /// `ReadingPaneView` renders from (CoreModels.swift), fed a `MessageDetail`
    /// round-tripped through the real FFI decode (`CastellumCore.getMessage`).
    func testOneClickFixtureShowsTheAffordance() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let mock = try OneShotMockServer()
        defer { mock.stop() }
        let id = try UnsubscribeFixtures.seedOneClick(seeded, httpsURL: mock.urlString)

        let detail = try model.core.getMessage(id: id)
        XCTAssertNotNil(detail)
        XCTAssertTrue(detail?.hasUnsubscribeAffordance ?? false)
    }

    /// Step 6's fixture, reused for step 2: mailto-only must ALSO show it.
    func testMailtoOnlyFixtureShowsTheAffordance() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let id = try UnsubscribeFixtures.seedMailtoOnly(seeded)

        let detail = try model.core.getMessage(id: id)
        XCTAssertNotNil(detail)
        XCTAssertTrue(detail?.hasUnsubscribeAffordance ?? false)
    }

    /// #3046: an https-only list WITHOUT the one-click flag has nothing the
    /// confirmation sheet can act on, so it must not offer the link at all.
    func testHttpsOnlyWithoutOneClickShowsNoAffordance() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let id = "\(seeded.accountId):m-https-only"
        try DirectFixture.seedMessage(
            profileDirectory: seeded.profileDirectory, id: id,
            accountId: seeded.accountId, mailboxId: seeded.mailboxId,
            subject: "Offers", fromAddress: "offers@example.com",
            listUnsubscribeMailto: nil,
            listUnsubscribeUrl: "https://example.com/unsub", listUnsubscribeOneClick: false)

        let detail = try XCTUnwrap(model.core.getMessage(id: id))
        XCTAssertFalse(detail.hasUnsubscribeAffordance)
    }

    /// Step 7: no List-Unsubscribe header at all -> no affordance.
    func testNoHeaderFixtureShowsNoAffordance() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let id = try UnsubscribeFixtures.seedNoHeader(seeded)

        let detail = try model.core.getMessage(id: id)
        XCTAssertNotNil(detail)
        XCTAssertFalse(detail?.hasUnsubscribeAffordance ?? true)
    }
}

// MARK: - Task 3: confirmation sheet + confirmed/cancelled outcomes
// (GATES.md steps 3, 4, 5, 6)

@MainActor
final class UnsubscribeConfirmSheetTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-unsub-confirm-\(UUID().uuidString)")
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

    /// Step 3: the sheet's candidate carries the EXACT seeded destination,
    /// byte-for-byte, and — before any confirm action — the mock server has
    /// received nothing. `UnsubscribeConfirmSheet` renders `detail.
    /// listUnsubscribeUrl` verbatim (see UnsubscribeConfirmSheet.swift), so
    /// asserting the candidate's URL IS the real render-time value.
    func testSheetCandidateShowsTheExactDestinationBeforeAnythingFires() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let mock = try OneShotMockServer()
        defer { mock.stop() }
        let id = try UnsubscribeFixtures.seedOneClick(seeded, httpsURL: mock.urlString)

        let detail = try XCTUnwrap(model.core.getMessage(id: id))
        model.unsubscribeCandidate = detail

        XCTAssertEqual(model.unsubscribeCandidate?.listUnsubscribeUrl, mock.urlString)
        XCTAssertTrue(model.unsubscribeCandidate?.listUnsubscribeOneClick ?? false)
        XCTAssertEqual(mock.requestCount, 0, "sheet must show the destination before anything fires")
    }

    /// Step 4, strengthened per the S66 plan's Assumption 5: `Cancel` in
    /// `UnsubscribeConfirmSheet` calls `dismiss()` ONLY — no `AppModel`
    /// method at all (see UnsubscribeConfirmSheet.swift's Cancel button).
    /// This test proves that invariant directly: showing the sheet and then
    /// dismissing it (never calling either confirm method) leaves the mock
    /// server at zero requests. This is NOT the same claim as a real
    /// packet-capture pass (see the human-only entry GATES.md keeps for
    /// that) — it only proves nothing reached THIS substituted URL.
    func testCancelLeavesTheMockServerAtZeroRequests() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let mock = try OneShotMockServer()
        defer { mock.stop() }
        let id = try UnsubscribeFixtures.seedOneClick(seeded, httpsURL: mock.urlString)

        let detail = try XCTUnwrap(model.core.getMessage(id: id))
        model.unsubscribeCandidate = detail
        // Cancel: the sheet's Cancel button only dismisses (see
        // UnsubscribeConfirmSheet.swift) — clearing the candidate is exactly
        // what that dismissal does to the `.sheet(item:)` binding.
        model.unsubscribeCandidate = nil

        XCTAssertEqual(mock.requestCount, 0)
    }

    /// Step 5: production one-click POSTs are https-only (ureq `https_only`,
    /// so an https->http redirect downgrade fails and no env/runtime override
    /// exists in Release). This mock is plain-HTTP loopback, so confirming
    /// must be REFUSED end to end: an error is surfaced, no status text, and
    /// zero requests reach the mock. The happy-path wire contract (exactly one
    /// POST, RFC 8058 body + content-type) is asserted at the transport level
    /// by the Rust test `posts_the_rfc_8058_body_and_content_type`
    /// (castellum-core unsubscribe.rs, via `post_one_click_with(url, false)`).
    func testConfirmOneClickRefusesPlainHttpAndSendsNothing() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let mock = try OneShotMockServer()
        defer { mock.stop() }
        let id = try UnsubscribeFixtures.seedOneClick(seeded, httpsURL: mock.urlString)

        let detail = try XCTUnwrap(model.core.getMessage(id: id))
        model.unsubscribeCandidate = detail
        XCTAssertEqual(mock.requestCount, 0)

        model.confirmUnsubscribeOneClick()

        try await waitUntil({ model.errorText != nil }, "the plain-http refusal never surfaced")
        XCTAssertNil(model.unsubscribeStatusText)
        XCTAssertEqual(mock.requestCount, 0, "plain http must never be POSTed to")
    }

    /// A non-one-click fixture must never be POSTed to even if somehow
    /// confirmed via the one-click path — `unsubscribe_one_click_json`
    /// returns `{"status":"unavailable"}` and touches no network
    /// (unsubscribe.rs:45-51). Belt-and-braces regression guard alongside
    /// the cancel test above.
    func testConfirmingAMailtoOnlyFixtureAsOneClickTouchesNoNetwork() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        let id = try UnsubscribeFixtures.seedMailtoOnly(seeded)

        let detail = try XCTUnwrap(model.core.getMessage(id: id))
        model.unsubscribeCandidate = detail
        model.confirmUnsubscribeOneClick()

        try await waitUntil({ model.unsubscribeStatusText != nil }, "status text was never set")
        XCTAssertEqual(model.unsubscribeStatusText, "This message has no one-click unsubscribe link.")
    }

    /// #3047: a mailto unsubscribe refused because the account is in
    /// evaluation mode must not leave a saved draft behind, must not claim
    /// success, and must raise the Enable Writes dialog.
    func testMailtoRefusedInEvaluationModeLeavesNoDraft() async throws {
        let model = makeModel()
        let seeded = try UnsubscribeFixtures.seedAccount(model: model, dir: model.profileDirectory)
        try model.accountStore.save(accountID: seeded.accountId, configJSON: """
        {"account_id":"\(seeded.accountId)","email":"test@local.test","host":"127.0.0.1",
         "port":3143,"username":"test@local.test","password":"secret","use_tls":false,
         "allow_invalid_certs":false,"smtp_host":"127.0.0.1","smtp_port":3025,
         "smtp_username":"test@local.test","smtp_password":"secret","smtp_use_tls":false}
        """)
        model.reloadAccounts()
        try await waitUntil({ model.accounts.contains { $0.id == seeded.accountId } },
                            "account never loaded")
        let id = try UnsubscribeFixtures.seedMailtoOnly(seeded)
        model.unsubscribeCandidate = try XCTUnwrap(model.core.getMessage(id: id))

        model.confirmUnsubscribeByMailto()

        try await waitUntil({ model.writeSafetyPrompt != nil }, "no Enable Writes dialog was raised")
        try await waitUntil({ (try? model.core.listDrafts(accountId: seeded.accountId))?.isEmpty == true },
                            "a refused unsubscribe left a saved draft")
        XCTAssertNil(model.unsubscribeStatusText, "a refusal must not report success")
    }
}

// MARK: - Task 3 (continued): mailto-only confirm -> exactly one Sent message
// (GATES.md step 6). Requires local GreenMail (see scripts/local-imap) —
// only `sync`/a real SMTP+IMAP round trip can put a message in a real Sent
// mailbox; skips (does not fail) when it's unreachable, the SAME idiom
// `UndoSendReopenTests.swift`/`UndoSendCrashHoldAppModelTests.swift` already
// use in this suite for a "real synced account" requirement. Every host used
// below is 127.0.0.1 (GreenMail's own compose port), so this still never
// leaves loopback.

@MainActor
final class UnsubscribeMailtoConfirmSentMessageTests: XCTestCase {
    private static let imapHost = "127.0.0.1"
    private static let imapPort: UInt16 = 3143
    private static let smtpPort: UInt16 = 3025

    private var models: [AppModel] = []

    private func makeModel(dir: URL) -> AppModel {
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

    private func greenMailReachable(port: UInt16) -> Bool {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr(Self.imapHost)
        return withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
    }

    func testConfirmMailtoOnlyResultsInExactlyOneSentMessage() async throws {
        try XCTSkipUnless(
            greenMailReachable(port: Self.imapPort) && greenMailReachable(port: Self.smtpPort),
            "GreenMail not running on \(Self.imapHost):\(Self.imapPort)/\(Self.smtpPort)")

        let dir = TestScratch.root
            .appendingPathComponent("castellum-unsub-mailto-sent-\(UUID().uuidString)")
        let accountId = "unsub-mailto-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        // Unique per run: GreenMail's "test@local.test" mailbox (and its
        // Sent folder) is a single server-side fixture shared across every
        // local test run against this container, not reset per test — so
        // "exactly one Sent message" must be scoped to THIS run's own
        // target address, never a raw count of the whole Sent folder.
        let targetAddress = "unsub-\(UUID().uuidString)@example.com"

        let model = makeModel(dir: dir)
        // Direct fixture (Task 1's offline seam) for the account row — the
        // mailto confirm path never reads a `messages` row at all
        // (`AppModel.confirmUnsubscribeByMailto` only needs `header.
        // accountId` and `listUnsubscribeMailto`), so this test needs no
        // seeded message, only a real account for the queued `send` op's
        // `operations.account_id REFERENCES accounts(id)` FK.
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: accountId,
            email: "test@local.test", mailboxId: mailboxId)
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: true)

        model.reloadAccounts()
        try await waitUntil({ model.accounts.contains { $0.id == accountId } },
            "reloadAccounts never picked up the direct-fixture account row")

        // deliverDraft sends with the account's STORED credentials.
        try model.accountStore.save(accountID: accountId, configJSON: """
        {"account_id":"\(accountId)","email":"test@local.test","host":"\(Self.imapHost)",
         "port":\(Self.imapPort),"username":"test@local.test","password":"secret",
         "use_tls":false,"allow_invalid_certs":false,
         "smtp_host":"\(Self.imapHost)","smtp_port":\(Self.smtpPort),
         "smtp_username":"test@local.test","smtp_password":"secret","smtp_use_tls":false}
        """)

        let candidate = MessageDetail(
            header: MessageHeader(
                id: "\(accountId):m-mailto", accountId: accountId, mailboxId: mailboxId,
                subject: "Product updates", fromName: nil, fromAddress: "updates@example.com",
                sentAt: nil, snippet: nil, hasAttachments: false, hasBody: nil,
                isRead: false, isStarred: false, threadCount: 1),
            rfc822MessageId: nil, addresses: [], textBody: nil, sanitizedHtml: nil,
            listUnsubscribeMailto: "mailto:\(targetAddress)?subject=unsubscribe&body=unsubscribe%20all")

        model.unsubscribeCandidate = candidate
        model.confirmUnsubscribeByMailto()
        try await waitUntil({ model.unsubscribeStatusText != nil || model.errorText != nil },
                            timeout: 15, "confirm never completed")
        // #3049: the text is only set once SMTP accepted the message, and
        // nothing is left queued for a later drain.
        XCTAssertEqual(model.unsubscribeStatusText, "Unsubscribe email sent.")
        XCTAssertNil(model.errorText)

        // Replay the resulting `sent_copy` op (IMAP APPEND to "Sent"), then
        // re-sync so the local `messages` table picks it up — mirrors
        // send.rs's own doc comment on how a sent copy becomes visible.
        let replayConfig = """
        {"account_id":"\(accountId)","host":"\(Self.imapHost)","port":\(Self.imapPort),
         "username":"test@local.test","password":"secret","use_tls":false,
         "allow_invalid_certs":false}
        """
        _ = try model.core.replayOperations(configJSON: replayConfig)

        let syncConfig = """
        {"account_id":"\(accountId)","email":"test@local.test","host":"\(Self.imapHost)",
         "port":\(Self.imapPort),"username":"test@local.test","password":"secret",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX","Sent"]}
        """
        _ = try model.core.syncAccount(configJSON: syncConfig)

        let mailboxes = try model.core.listMailboxes(accountId: accountId)
        let sent = try XCTUnwrap(
            mailboxes.first { $0.name.caseInsensitiveCompare("Sent") == .orderedSame },
            "GreenMail never auto-created a Sent mailbox for the replayed sent_copy")

        let sentMessages = try model.core.listMessages(
            accountId: accountId, mailboxId: sent.id, limit: 50, offset: 0)
        // Scoped to this run's unique target address, not a raw folder
        // count (see targetAddress's own comment above) — but the message
        // that confirmUnsubscribeByMailto queued must still be the ONLY one
        // addressed to it.
        let matching = try sentMessages.filter { header in
            let detail = try model.core.getMessage(id: header.id)
            return detail?.addresses.contains { $0.kind == "to" && $0.address == targetAddress }
                ?? false
        }
        XCTAssertEqual(matching.count, 1, "exactly one new Sent message addressed to \(targetAddress)")
    }
}
