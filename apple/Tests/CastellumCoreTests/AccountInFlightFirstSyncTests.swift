// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// PR #158 GUI check (2026-09-26): Marc added a 33k-message GreenMail account
/// five times, pressing "Continue in Background" each time. The last two
/// looked "silently abandoned": partial mail, `last_attempt_at` but no
/// success, no error, no log line, and Re-authenticate… showed an empty,
/// unconfigured form.
///
/// They were never dropped — a sample of the live process showed all five
/// `castellum_sync_account` calls still running (starved on the shared DB
/// mutex and a busy GreenMail). The real defect is WHERE `addAccount` stored
/// the account's config: only in `performSync`'s `onSuccess`, i.e. once the
/// WHOLE first sync returned. The core creates the `accounts` row at the
/// very start of a sync, and the progress poller makes the account visible
/// as soon as that row exists, so for the minutes a large first sync takes
/// the account was listed with NO stored config:
///   * Re-authenticate prefills from `storedConfig(for:)` → empty form;
///   * `reSyncAll` (launch) and the periodic scheduler walk
///     `accountStore.allAccountIDs()` → the account is invisible to both;
///   * quit or crash before the sync returns (Marc's session ended exactly
///     so at 10:15:58Z with both syncs still at 9.5k/8.4k messages) → the
///     account is orphaned for good: it stays half-synced, with nothing ever
///     retrying it or recording an outcome.
///
/// Both tests hold the first sync AFTER login/LIST/SELECT with a scripted
/// loopback IMAP server (`StallingImapServer`), so "still in flight" is a
/// fact the test controls. No GreenMail, no real Keychain (TestHost routes
/// `AccountStore` to the in-memory backing).
@MainActor
final class AccountInFlightFirstSyncTests: XCTestCase {
    private var models: [AppModel] = []
    private var servers: [StallingImapServer] = []

    private func makeModel(profile dir: URL? = nil) -> (AppModel, URL) {
        let dir = dir ?? TestScratch.root
            .appendingPathComponent("castellum-inflight-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return (model, dir)
    }

    private func makeServer() throws -> StallingImapServer {
        let server = try StallingImapServer()
        servers.append(server)
        return server
    }

    override func tearDown() async throws {
        for server in servers { server.stop() } // wakes any held session
        for model in models {
            let deadline = Date().addingTimeInterval(10)
            while model.isSyncing && Date() < deadline {
                try await Task.sleep(nanoseconds: 50_000_000)
            }
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        servers = []
        try await super.tearDown()
    }

    private func config(accountId: String, user: String, port: UInt16) -> String {
        """
        {"account_id":"\(accountId)","email":"\(user)","host":"127.0.0.1",
         "port":\(port),"username":"\(user)","password":"pw",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
    }

    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 10,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    /// Account A's first sync is in flight in the background (past login,
    /// listed in the sidebar — the "Continue in Background" state); account
    /// B is added and completes meanwhile. A must already be configured
    /// (Re-authenticate/relaunch/scheduler can see it) while in flight, and
    /// must still complete and be recorded once its server answers.
    func testInFlightFirstSyncIsConfiguredAndStillCompletesWhileAnotherAccountIsAdded() async throws {
        let server = try makeServer()
        let (model, _) = makeModel()
        let userA = "a@mock.test", userB = "b@mock.test"
        let idA = "inflight-a-\(UUID().uuidString)"
        let idB = "inflight-b-\(UUID().uuidString)"
        let configA = config(accountId: idA, user: userA, port: server.port)
        let configB = config(accountId: idB, user: userB, port: server.port)
        server.hold(user: userA)

        model.addAccount(accountID: idA, configJSON: configA)
        try await waitUntil(
            { server.isStalled(user: userA) && model.accounts.contains { $0.id == idA } },
            "A's first sync never reached the held descriptor fetch / never became visible")

        // Add B while A is still running.
        var bResult: AccountSyncStatus?
        model.addAccount(accountID: idB, configJSON: configB, completion: { bResult = $0 })
        try await waitUntil({ bResult != nil }, "B's add never completed")
        XCTAssertEqual(bResult?.state, .ok)
        XCTAssertEqual(model.storedConfig(for: idB), configB)

        // A is still genuinely in flight — and must already be configured.
        XCTAssertTrue(server.isStalled(user: userA), "A's sync should still be held")
        XCTAssertEqual(model.accountSyncStatuses[idA]?.state, .syncing)
        try await waitUntil(
            { model.storedConfig(for: idA) == configA },
            "A is listed but has no stored config while its first sync runs — "
                + "Re-authenticate shows an empty form and nothing can resume it")
        XCTAssertTrue(try model.accountStore.allAccountIDs().contains(idA))

        // Let A finish: it completes and its success is recorded, durably.
        server.release(user: userA)
        try await waitUntil(
            { model.accountSyncStatuses[idA]?.state == .ok },
            "A's in-flight first sync never completed")
        XCTAssertEqual(model.storedConfig(for: idA), configA)
        let diagnostics = try model.core.localDiagnostics().accounts
        XCTAssertNotNil(
            diagnostics.first { $0.accountId == idA }?.lastSuccessAt,
            "A's completed first sync was not recorded as a success")
    }

    /// Merge of #163 (pre-sync IMAP+SMTP probe) with this PR: the Add-account
    /// sheet now reaches `addAccount` only through `addAccountWithPreflight`,
    /// so the early store must still fire on THAT path — the probe proves
    /// the credentials, `addAccount`'s `onLoggedIn` then persists them the
    /// moment the real sync is live, not once it returns. The probe is faked
    /// (`connectionProbeOverride`, as in `AccountAutoProbeTests`) because the
    /// loopback server speaks IMAP only; the sync itself is real and held.
    func testPreflightedFirstSyncIsConfiguredWhileStillInFlight() async throws {
        let server = try makeServer()
        let (model, _) = makeModel()
        let user = "preflight@mock.test"
        let id = "inflight-preflight-\(UUID().uuidString)"
        let cfg = config(accountId: id, user: user, port: server.port)
        server.hold(user: user)
        model.connectionProbeOverride = { _ in
            .success(ConnectionTestReport(
                imap: ConnectionLegResult(ok: true, errorKind: nil, summary: nil),
                smtp: ConnectionLegResult(ok: true, errorKind: nil, summary: nil),
                mailboxes: []))
        }

        var probePassed = false
        var result: AccountSyncStatus?
        model.addAccountWithPreflight(
            accountID: id,
            connectionConfig: AccountConfig(
                accountId: id, email: user, host: "127.0.0.1", port: Int(server.port),
                username: user, password: "pw", useTls: false, allowInvalidCerts: false,
                smtpHost: nil, smtpPort: nil, smtpUsername: nil, smtpPassword: nil,
                smtpUseTls: nil, displayName: nil),
            fullConfigJSON: cfg,
            onProbePassed: { probePassed = true },
            completion: { result = $0 })

        try await waitUntil(
            { server.isStalled(user: user) && model.accounts.contains { $0.id == id } },
            "the preflighted first sync never reached the held descriptor fetch / never became visible")
        XCTAssertTrue(probePassed, "the probe gate must have cleared before the sync started")
        XCTAssertNil(result, "the held sync must not have completed yet")
        XCTAssertTrue(server.isStalled(user: user))
        try await waitUntil(
            { model.storedConfig(for: id) == cfg },
            "preflight path: account listed but no stored config while its first sync runs")
        XCTAssertTrue(try model.accountStore.allAccountIDs().contains(id))

        server.release(user: user)
        try await waitUntil({ result?.state == .ok }, "the held first sync never completed")
        XCTAssertEqual(model.storedConfig(for: id), cfg)
    }

    /// The same in-flight first sync, but the app goes away before it
    /// returns (quit/crash — what ended Marc's session with test33/test55
    /// half-synced). On relaunch, the launch re-sync (`reSyncAll`) must pick
    /// the account up and finish it, recording the outcome — not leave it
    /// orphaned with partial mail and no stored config forever.
    func testFirstSyncInterruptedBeforeReturningIsResumedOnRelaunch() async throws {
        let server = try makeServer()
        let (first, dir) = makeModel()
        let user = "relaunch@mock.test"
        let id = "inflight-relaunch-\(UUID().uuidString)"
        let cfg = config(accountId: id, user: user, port: server.port)
        server.hold(user: user) // one-shot: holds only the first session

        first.addAccount(accountID: id, configJSON: cfg)
        try await waitUntil(
            { server.isStalled(user: user) && first.accounts.contains { $0.id == id } },
            "the first sync never reached the held descriptor fetch / never became visible")
        // Give the in-flight state a moment to settle into whatever it
        // durably leaves behind (a few poller ticks).
        try await Task.sleep(nanoseconds: 1_500_000_000)

        // "Relaunch": a fresh model on the same profile + Keychain namespace,
        // running the launch-time re-sync, while the first process's sync
        // never returned.
        let (second, _) = makeModel(profile: dir)
        second.reSyncAll()
        try await waitUntil(
            { second.accountSyncStatuses[id]?.state == .ok },
            "the interrupted first sync was never resumed on relaunch — the account is orphaned")
        XCTAssertEqual(server.descriptorFetchCount(user: user), 1, "the relaunch's own pass was served")
        XCTAssertNotNil(
            try second.core.localDiagnostics().accounts.first { $0.accountId == id }?.lastSuccessAt)

        server.release(user: user, disconnect: true) // end the first model's held sync
    }
}
