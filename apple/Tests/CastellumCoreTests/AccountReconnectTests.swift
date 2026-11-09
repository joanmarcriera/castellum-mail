// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

///: AccountSetupSheet's inline Reconnect is a thin wrapper over
/// `AppModel.addAccount(completion:)` and `saveAccountWithoutSync` — these
/// tests exercise that reducer directly (no GUI harness drives sheets), the
/// same way SyncSpinnerTests exercises `sync`/`addAccount`.
///
/// The wrong-then-right-password path requires local GreenMail seeded (see
/// `scripts/local-imap`, account `test@local.test:secret` from
/// `docker-compose.yml`'s `greenmail.users`, a fixed-password preset account
/// — unlike SMTP-auto-created accounts, IMAP LOGIN against it genuinely
/// enforces the password). Skips (not fails) if GreenMail isn't running, so
/// CI without Docker stays green.
@MainActor
final class AccountReconnectTests: XCTestCase {
    private static let host = "127.0.0.1"
    private static let port: UInt16 = 3143

    /// Models made by this test case, so tearDown can remove their Keychain items.
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-reconnect-\(UUID().uuidString)")
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

    private func config(accountId: String, password: String) -> String {
        """
        {"account_id":"\(accountId)","email":"test@local.test","host":"\(Self.host)",
         "port":\(Self.port),"username":"test@local.test","password":"\(password)",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
    }

    /// Polls a completion-driven result to a Bool flag; fails explicitly on
    /// timeout (same idiom as SyncSpinnerTests' waitUntilNotSyncing).
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

    /// The sheet's inline reconnect: wrong password reports a classified
    /// `.authenticationFailed` result (not a thrown modal error) and does
    /// NOT persist credentials on its own — "Save anyway" is a distinct,
    /// explicit action. The right password then reports `.ok` and the
    /// account's config becomes readable — the reconnect actually worked.
    ///
    /// Re-authenticate only ever targets an EXISTING account, so the account
    /// is added (right password) first — this also establishes the durable
    /// diagnostics row a fresh, never-synced account doesn't have yet, which
    /// `performSync`'s catch block documents as the one case that falls back
    /// to a generic "network" kind rather than the real one (see AppModel.
    /// performSync's "first-ever connect" comment); Re-authenticate's wrong
    /// password below is exactly the already-has-a-row case that DOES report
    /// the true `auth` kind.
    func testWrongPasswordThenRightPasswordReconnectFlow() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let accountId = "reconnect-\(UUID().uuidString)"

        var addResult: AccountSyncStatus?
        model.addAccount(
            accountID: accountId,
            configJSON: config(accountId: accountId, password: "secret"),
            completion: { addResult = $0 }
        )
        try await waitUntil({ addResult != nil }, "initial add never completed")
        XCTAssertEqual(addResult?.state, .ok)
        let configAfterAdd = model.storedConfig(for: accountId)
        XCTAssertNotNil(configAfterAdd)

        // Re-authenticate with the WRONG password: the sheet's inline result
        // must be a classified auth failure, and — since Reconnect doesn't
        // save on failure — the Keychain must still hold the GOOD config.
        var wrongResult: AccountSyncStatus?
        model.addAccount(
            accountID: accountId,
            configJSON: config(accountId: accountId, password: "wrong-password"),
            completion: { wrongResult = $0 }
        )
        try await waitUntil({ wrongResult != nil }, "wrong-password reconnect never completed")
        XCTAssertEqual(wrongResult?.state, .authenticationFailed)
        XCTAssertEqual(
            model.storedConfig(for: accountId), configAfterAdd,
            "a failed reconnect must not overwrite the still-good stored credentials")

        // Re-authenticate again with the RIGHT password: succeeds and clears
        // the failed status the wrong attempt just set.
        var rightResult: AccountSyncStatus?
        model.addAccount(
            accountID: accountId,
            configJSON: config(accountId: accountId, password: "secret"),
            completion: { rightResult = $0 }
        )
        try await waitUntil({ rightResult != nil }, "right-password reconnect never completed")
        XCTAssertEqual(rightResult?.state, .ok)
        XCTAssertEqual(model.accountSyncStatuses[accountId]?.state, .ok)
    }

    ///: cancelling a stuck Add-account sheet and retrying for
    /// the same address does NOT cancel the first attempt's background sync
    /// (`AccountSetupSheet`'s Cancel/"Continue in Background" only dismiss
    /// the sheet) — `performSync`'s `syncGenerations` counter used to drop
    /// that first attempt's eventual completion entirely once the retry
    /// bumped the generation: no credential save, no `reloadAccounts`, no
    /// error shown, even though the original sync genuinely succeeded.
    ///
    /// Calling `addAccount` twice back-to-back on MainActor, with no `await`
    /// in between, reproduces the race deterministically rather than by
    /// timing luck: `performSync` bumps `syncGenerations[accountId]`
    /// synchronously (before spawning its `Task.detached` work), so the
    /// SECOND call always claims the higher generation regardless of which
    /// attempt's background work finishes first — the first (original)
    /// attempt's completion is a stale-generation completion by
    /// construction, not by timing luck.
    ///
    /// The retry points at an unreachable host (127.0.0.1:1, nothing
    /// listening — same fixture `SyncSpinnerTests` uses) rather than a wrong
    /// password: this is a brand-new account with no durable diagnostics row
    /// yet, and `performSync`'s catch block documents that a first-ever
    /// connect's error kind falls back to generic "network" classification
    /// without one (see `AccountReconnectTests`' doc comment above), so an
    /// auth-vs-network distinction here would be undermined by an unrelated,
    /// already-documented gotcha. An unreachable host fails fast and
    /// classifies deterministically as `.unreachable` either way, which is
    /// all this test needs from the retry.
    func testOriginalSyncSurvivesConcurrentRetryWithDifferentGeneration() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let accountId = "stale-gen-\(UUID().uuidString)"
        let originalConfig = config(accountId: accountId, password: "secret")
        let retryConfig = """
        {"account_id":"\(accountId)","email":"t@local.test","host":"127.0.0.1","port":1,
         "username":"t@local.test","password":"x","use_tls":false,
         "allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """

        // No `completion` closures here on purpose: `addAccount`'s own
        // completion callback is deliberately conservative (it only fires
        // once `accountSyncStatuses` agrees the account is `.ok` — see its
        // "only report success once" comment) and isn't the signal this
        // test needs. What matters is the DURABLE side effect of a
        // completed sync — credential save + `reloadAccounts` — which must
        // run regardless of whether the UI ever hears about it.
        model.addAccount(accountID: accountId, configJSON: originalConfig)
        model.addAccount(accountID: accountId, configJSON: retryConfig)

        try await waitUntil({ !model.isSyncing }, timeout: 20, "both syncs never settled")

        // The retry is the latest generation and unreachable — its failure
        // must be the authoritative status shown.
        XCTAssertEqual(model.accountSyncStatuses[accountId]?.state, .unreachable)

        // The original (now stale-generation) sync used the correct
        // GreenMail credentials and genuinely succeeded — that result must
        // still be honored: the account becomes visible and its credentials
        // are persisted, exactly as if no retry had ever happened. Before
        // the fix, `performSync`'s generation guard silently dropped
        // `onSuccess` (and with it the credential save) entirely here.
        try await waitUntil(
            { model.accounts.contains { $0.id == accountId } },
            "the original sync's completion was discarded — the account never appeared"
        )
        XCTAssertEqual(
            model.storedConfig(for: accountId), originalConfig,
            "the original (stale-generation) sync's credential save was discarded")
    }

    /// PR #158 follow-up: `performSync`'s `onSuccess`/`onFailure` always run
    /// regardless of generation (the #2495 fix above), but `addAccount`'s
    /// `onSuccess` saves ITS OWN captured `configJSON` unconditionally — with
    /// no guard, an out-of-order pair of SUCCESSFUL attempts (a retry that
    /// finishes first, then the original finishing later) silently regresses
    /// the Keychain back to the older, stale config even though the UI is
    /// showing the retry's `.ok` result.
    ///
    /// Unlike the test above, both attempts here must genuinely SUCCEED, so
    /// there is no safe "one of them fails fast" speed differential to lean
    /// on for ordering. A `GatedProxy` (TestSupport) holds the ORIGINAL
    /// attempt's connection open — accepted, but no bytes flow — until this
    /// test has already confirmed the RETRY's save landed; only then does it
    /// release the original through to real GreenMail. The completion order
    /// is proven by that release, not raced.
    func testOutOfOrderCompletedSyncsSaveTheNewestGeneration() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let model = makeModel()
        let accountId = "out-of-order-\(UUID().uuidString)"
        let gate = try GatedProxy(upstreamHost: Self.host, upstreamPort: Self.port)

        // The original attempt: routed through the gate, so its connection
        // is accepted but held — no LOGIN response arrives until `release()`.
        let originalConfig = """
        {"account_id":"\(accountId)","email":"test@local.test","host":"\(Self.host)",
         "port":\(gate.port),"username":"test@local.test","password":"secret",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
        // The retry: a corrected folder scope, talking directly to real
        // GreenMail — nothing holds it back.
        let retryConfig = """
        {"account_id":"\(accountId)","email":"test@local.test","host":"\(Self.host)",
         "port":\(Self.port),"username":"test@local.test","password":"secret",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX","Sent"]}
        """

        // Back-to-back, no `await` in between: the original claims the
        // lower generation, the retry the higher one — exactly like the
        // test above, deterministic by call order regardless of which
        // attempt's background work finishes first.
        model.addAccount(accountID: accountId, configJSON: originalConfig)
        model.addAccount(accountID: accountId, configJSON: retryConfig)

        // The retry is unheld, so it completes and saves first.
        try await waitUntil(
            { model.storedConfig(for: accountId) == retryConfig },
            "the retry (newer generation) never completed and saved"
        )
        XCTAssertEqual(model.accountSyncStatuses[accountId]?.state, .ok)

        // Only now — after the newer generation's save has genuinely
        // landed — let the held original attempt proceed. It also succeeds,
        // but it is a STALE, superseded completion: it must not be allowed
        // to overwrite the newer saved config.
        gate.release()
        try await waitUntil({ !model.isSyncing }, timeout: 20, "the held original sync never settled")

        XCTAssertEqual(
            model.storedConfig(for: accountId), retryConfig,
            "a stale-but-completed sync overwrote the newer saved config")
    }

    /// "Save anyway": stores the config without a successful
    /// sync, and deliberately leaves `accountSyncStatuses` untouched so the
    /// account keeps showing the failed state from the attempt.
    ///
    /// Regression test: "Save anyway" used to only write the Keychain entry
    /// and never the core's `accounts` row (which `list_accounts`/`reloadAccounts`
    /// reads) — `core.listAccounts()` only ever got an account row as a side
    /// effect of a SUCCESSFUL sync, so a save after a failed connection check
    /// left the Keychain holding valid credentials for an account that never
    /// appeared anywhere in the UI. Asserting `model.accounts` here (not just
    /// `storedConfig`) is what would have caught that.
    func testSaveAnywayPersistsConfigWithoutTouchingSyncStatus() async throws {
        let model = makeModel()
        let accountId = "save-anyway-\(UUID().uuidString)"
        XCTAssertNil(model.accountSyncStatuses[accountId])

        model.saveAccountWithoutSync(
            accountID: accountId,
            configJSON: config(accountId: accountId, password: "whatever"))

        XCTAssertNotNil(model.storedConfig(for: accountId), "config must be persisted")
        XCTAssertNil(
            model.accountSyncStatuses[accountId],
            "Save anyway must not fabricate a sync status")
        XCTAssertNil(model.errorText)

        try await waitUntil(
            { model.accounts.contains { $0.id == accountId } },
            "account saved via 'Save anyway' never appeared in model.accounts"
        )
    }

    /// Save anyway reports failure (so the sheet stays open and keeps the
    /// typed passwords) when the core rejects the config.
    func testSaveAccountWithoutSyncReturnsFalseOnFailure() {
        let model = makeModel()
        let ok = model.saveAccountWithoutSync(accountID: "bad-\(UUID().uuidString)", configJSON: "not json")
        XCTAssertFalse(ok)
        XCTAssertNotNil(model.errorText)
    }

    func testSaveAccountWithoutSyncReturnsTrueOnSuccess() {
        let model = makeModel()
        let id = "ok-\(UUID().uuidString)"
        XCTAssertTrue(model.saveAccountWithoutSync(accountID: id, configJSON: config(accountId: id, password: "x")))
    }

    func testSheetButtonRules() {
        XCTAssertTrue(AccountSheetButtons.showsSaveAnyway(isFailure: true, isReauth: false))
        XCTAssertFalse(AccountSheetButtons.showsSaveAnyway(isFailure: true, isReauth: true))
        XCTAssertFalse(AccountSheetButtons.showsSaveAnyway(isFailure: false, isReauth: false))
        XCTAssertTrue(AccountSheetButtons.canSave(fieldsComplete: true, isConnecting: false, isSuccess: false, isTesting: false))
        XCTAssertFalse(AccountSheetButtons.canSave(fieldsComplete: true, isConnecting: false, isSuccess: true, isTesting: false))
        // RP12: room for "Continue in Background" while connecting.
        XCTAssertFalse(AccountSheetButtons.footerShowsEditActions(isConnecting: true))
        XCTAssertTrue(AccountSheetButtons.footerShowsEditActions(isConnecting: false))
    }
}
