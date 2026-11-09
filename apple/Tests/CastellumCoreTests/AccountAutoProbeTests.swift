// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

///: Add Account (and Re-authenticate) must probe IMAP + SMTP
/// BEFORE the first sync — fail fast on a bad password/host rather than
/// discovering it after a long first sync. `AppModel.addAccountWithPreflight`
/// is the reducer this exercises directly (no SwiftUI, no `AccountSetupSheet`
/// — same idiom as `AccountReconnectTests`/`SyncSpinnerTests`).
///
/// The probe itself (`connectionProbeOverride`) is FAKED here — no GreenMail,
/// no real Keychain — so these tests are deterministic and don't depend on
/// Docker being up. `AccountConnectionTestTests` already covers the real FFI
/// round trip (`castellum_test_connection`) for both legs; this suite only
/// covers the NEW gating decision: whether `addAccount` (the real,
/// unmodified reducer — this suite never touches its body) runs at all.
/// A "proceeds" assertion below therefore points `addAccount` at an
/// unreachable host (127.0.0.1:1, the same fixture `SyncSpinnerTests` uses)
/// and checks the sync genuinely started (`isSyncing` flips true) and
/// finished with a real, independently-classified `.unreachable` — i.e. the
/// gate got out of the way and let the ordinary sync path run, without this
/// test needing a live IMAP/SMTP server anywhere.
@MainActor
final class AccountAutoProbeTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-autoprobe-\(UUID().uuidString)")
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

    /// Nothing listens on 127.0.0.1:1 — `addAccount`'s real sync fails fast
    /// and deterministically with `.unreachable` (see `SyncSpinnerTests`).
    private func unreachableConfig(accountId: String) -> String {
        """
        {"account_id":"\(accountId)","email":"t@local.test","host":"127.0.0.1","port":1,
         "username":"t@local.test","password":"x","use_tls":false,
         "allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
    }

    private func connectionConfig(accountId: String, host: String = "imap.example.com") -> AccountConfig {
        AccountConfig(
            accountId: accountId, email: "t@local.test", host: host, port: 993,
            username: "t@local.test", password: "x", useTls: true,
            allowInvalidCerts: false, smtpHost: nil, smtpPort: nil,
            smtpUsername: nil, smtpPassword: nil, smtpUseTls: nil, displayName: nil)
    }

    private func passingReport() -> ConnectionTestReport {
        ConnectionTestReport(
            imap: ConnectionLegResult(ok: true, errorKind: nil, summary: nil),
            smtp: ConnectionLegResult(ok: true, errorKind: nil, summary: nil),
            mailboxes: [])
    }

    private func failingReport(imapOk: Bool, imapKind: String?, smtpOk: Bool = true) -> ConnectionTestReport {
        ConnectionTestReport(
            imap: ConnectionLegResult(ok: imapOk, errorKind: imapOk ? nil : imapKind, summary: imapOk ? nil : "x"),
            smtp: ConnectionLegResult(ok: smtpOk, errorKind: smtpOk ? nil : "auth", summary: smtpOk ? nil : "x"),
            mailboxes: [])
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

    /// A failing probe (bad IMAP password) must block persistence AND the
    /// sync — `addAccount` never runs — and must name which leg failed via
    /// the untouched `errorKind`, exactly what `ConnectionTestPresentation`
    /// (the button's own presentation layer) keys its inline wording off.
    func testProbeFailureBlocksPersistenceAndSyncAndNamesTheFailingLeg() async throws {
        let model = makeModel()
        let accountId = "probe-fail-\(UUID().uuidString)"
        let report = failingReport(imapOk: false, imapKind: "auth")
        model.connectionProbeOverride = { _ in .success(report) }

        var probeSeen: ConnectionTestReport?
        var completionFired = false
        model.addAccountWithPreflight(
            accountID: accountId,
            connectionConfig: connectionConfig(accountId: accountId),
            fullConfigJSON: unreachableConfig(accountId: accountId),
            onProbeComplete: { probeSeen = $0 },
            completion: { _ in completionFired = true }
        )

        // `onProbeComplete` firing is the deterministic signal that the gate
        // has evaluated this report: the only code after it on this path is
        // a synchronous guard-and-return (no further `await`), so by the
        // time this observes `probeSeen != nil` the "don't call addAccount"
        // decision has already been made — no fixed sleep needed.
        try await waitUntil({ probeSeen != nil }, "the probe never completed")
        XCTAssertFalse(model.isSyncing, "a failed probe must never start the sync")
        XCTAssertFalse(completionFired, "addAccount must never run after a failing probe")
        XCTAssertTrue(model.accounts.isEmpty, "nothing must be persisted after a failing probe")
        XCTAssertNil(model.storedConfig(for: accountId), "no credentials must be saved after a failing probe")
        XCTAssertEqual(probeSeen?.imap.ok, false)
        XCTAssertEqual(probeSeen?.imap.errorKind, "auth", "the failing leg's reason must reach the caller untouched")
        XCTAssertEqual(probeSeen?.smtp?.ok, true, "only the IMAP leg failed — SMTP's own result must still be reported")
    }

    /// A passing probe must let the real, unmodified `addAccount` reducer
    /// run exactly as it does today — proven by pointing it at an
    /// unreachable host and observing the ordinary sync lifecycle (spinner
    /// up, then a genuine `.unreachable` classification from the real sync
    /// path, not a probe-shaped stand-in).
    func testProbeSuccessProceedsToTheRealSync() async throws {
        let model = makeModel()
        let accountId = "probe-pass-\(UUID().uuidString)"
        model.connectionProbeOverride = { _ in .success(self.passingReport()) }

        var probeSeen: ConnectionTestReport?
        var result: AccountSyncStatus?
        model.addAccountWithPreflight(
            accountID: accountId,
            connectionConfig: connectionConfig(accountId: accountId),
            fullConfigJSON: unreachableConfig(accountId: accountId),
            onProbeComplete: { probeSeen = $0 },
            completion: { result = $0 }
        )

        // 127.0.0.1:1 refuses the connection near-instantly, so the sync's
        // begin/end can both land inside one polling tick — asserting a
        // transient `isSyncing == true` window here would be a race. Its
        // OUTCOME is the deterministic proof the gate let the real sync run
        // at all (a blocked probe never even calls `addAccount` — see the
        // failure test above, which asserts `completion` never fires).
        try await waitUntil({ result != nil }, "the real sync's completion never fired")
        XCTAssertEqual(result?.state, .unreachable, "the REAL sync path classified this, not the probe")
        XCTAssertEqual(probeSeen?.imap.ok, true)
    }

    /// "Don't probe twice": a Test Connection that already passed for
    /// byte-identical settings must not be re-run when Sync is clicked.
    func testPriorPassingTestWithIdenticalSettingsSkipsTheProbe() async throws {
        let model = makeModel()
        let accountId = "probe-skip-\(UUID().uuidString)"
        let config = connectionConfig(accountId: accountId)

        var probeCallCount = 0
        model.connectionProbeOverride = { _ in
            probeCallCount += 1
            return .success(self.passingReport())
        }

        var result: AccountSyncStatus?
        model.addAccountWithPreflight(
            accountID: accountId,
            connectionConfig: config,
            fullConfigJSON: unreachableConfig(accountId: accountId),
            priorTest: (report: passingReport(), tested: config),
            completion: { result = $0 }
        )

        try await waitUntil({ result != nil }, "addAccount must still run on the skip path")
        XCTAssertEqual(result?.state, .unreachable)
        XCTAssertEqual(probeCallCount, 0, "an identical prior pass must not trigger a second probe")
    }

    /// Editing a field after a passing Test Connection must force a fresh
    /// probe on the next Sync — a stale pass must never be trusted.
    func testChangedSettingsAfterAPassingTestForcesAReProbe() async throws {
        let model = makeModel()
        let accountId = "probe-reprobe-\(UUID().uuidString)"
        let staleTested = connectionConfig(accountId: accountId, host: "imap.old.example.com")
        let current = connectionConfig(accountId: accountId, host: "imap.new.example.com")

        var probeCallCount = 0
        model.connectionProbeOverride = { _ in
            probeCallCount += 1
            return .success(self.passingReport())
        }

        var result: AccountSyncStatus?
        model.addAccountWithPreflight(
            accountID: accountId,
            connectionConfig: current,
            fullConfigJSON: unreachableConfig(accountId: accountId),
            priorTest: (report: passingReport(), tested: staleTested),
            completion: { result = $0 }
        )

        try await waitUntil({ result != nil }, "addAccount must still run once the re-probe passes")
        XCTAssertEqual(result?.state, .unreachable)
        XCTAssertEqual(probeCallCount, 1, "settings changed since the prior pass — it must re-probe")
    }

    /// The probe REQUEST itself failing (not a leg failing) — e.g. a core
    /// error unrelated to network/auth — must also block the sync, and reach
    /// the caller via `onProbeRequestFailed`, mirroring the manual Test
    /// Connection button's own `.error` case.
    func testProbeRequestFailureBlocksTheSync() async throws {
        let model = makeModel()
        let accountId = "probe-reqfail-\(UUID().uuidString)"
        model.connectionProbeOverride = { _ in .failure(CastellumError.internalError) }

        var requestError: Error?
        var completionFired = false
        model.addAccountWithPreflight(
            accountID: accountId,
            connectionConfig: connectionConfig(accountId: accountId),
            fullConfigJSON: unreachableConfig(accountId: accountId),
            onProbeRequestFailed: { requestError = $0 },
            completion: { _ in completionFired = true }
        )

        // `onProbeRequestFailed` firing is the deterministic "this attempt
        // is done" signal on this path — nothing else follows it in
        // `addAccountWithPreflight`'s `.failure` case.
        try await waitUntil({ requestError != nil }, "the probe request never reported failure")
        XCTAssertFalse(completionFired, "addAccount must never run when the probe request itself fails")
        XCTAssertTrue(model.accounts.isEmpty)
    }

    /// CRITICAL (found in review of PR #163): while the probe is in flight,
    /// the sheet's `phase` stays `.editing`, so Cancel only calls `dismiss()`
    /// — nothing tells `addAccountWithPreflight` the attempt is no longer
    /// wanted. A manual Test Connection never persists anything, so a
    /// cancelled Sync-triggered probe that later resolves as PASSING must
    /// not persist either. This gates a fake probe with a continuation so
    /// the test can cancel mid-flight, THEN let it resolve as passing, and
    /// asserts nothing downstream of the cancellation ever ran.
    func testCancellingWhileTheProbeIsInFlightNeverSavesEvenIfItLaterPasses() async throws {
        let model = makeModel()
        let accountId = "probe-cancel-\(UUID().uuidString)"

        var wanted = true
        var releaseProbe: ((Result<ConnectionTestReport, Error>) -> Void)?
        model.connectionProbeOverride = { _ in
            await withCheckedContinuation { (continuation: CheckedContinuation<Result<ConnectionTestReport, Error>, Never>) in
                releaseProbe = { continuation.resume(returning: $0) }
            }
        }

        var probeSeen: ConnectionTestReport?
        var probePassedFired = false
        var completionFired = false
        model.addAccountWithPreflight(
            accountID: accountId,
            connectionConfig: connectionConfig(accountId: accountId),
            fullConfigJSON: unreachableConfig(accountId: accountId),
            isStillWanted: { wanted },
            onProbeComplete: { probeSeen = $0 },
            onProbePassed: { probePassedFired = true },
            completion: { _ in completionFired = true }
        )

        // Wait for the probe to actually be in flight (the override has
        // been called and is now suspended on the continuation) before
        // cancelling — cancelling before it starts would prove nothing
        // about a probe that finishes AFTER cancellation.
        try await waitUntil({ releaseProbe != nil }, "the probe never started")
        wanted = false // simulates Cancel/dismiss while the probe is running
        releaseProbe?(.success(passingReport())) // ... which then passes

        // `onProbeComplete` fires unconditionally, even on the now-cancelled
        // path, and nothing after it in `addAccountWithPreflight` suspends
        // again before the `isStillWanted` re-check — so this is still a
        // precise, non-sleep-based synchronization point.
        try await waitUntil({ probeSeen != nil }, "the probe never completed")
        XCTAssertEqual(probeSeen?.imap.ok, true, "sanity: the probe really did pass")
        XCTAssertFalse(probePassedFired, "a cancelled attempt must never signal 'proceeding to sync'")
        XCTAssertFalse(completionFired, "addAccount must never run once the attempt was cancelled")
        XCTAssertFalse(model.isSyncing, "no sync must start for a cancelled attempt")
        XCTAssertTrue(model.accounts.isEmpty, "nothing must be persisted for a cancelled attempt")
        XCTAssertNil(model.storedConfig(for: accountId), "no credentials must be saved for a cancelled attempt")
    }

    /// The skip path (identical prior pass) must be cancellable too, even
    /// though it never touches the probe at all.
    func testCancellingBeforeTheSkipPathPersistsNeverSaves() async throws {
        let model = makeModel()
        let accountId = "probe-cancel-skip-\(UUID().uuidString)"
        let config = connectionConfig(accountId: accountId)

        var completionFired = false
        model.addAccountWithPreflight(
            accountID: accountId,
            connectionConfig: config,
            fullConfigJSON: unreachableConfig(accountId: accountId),
            priorTest: (report: passingReport(), tested: config),
            isStillWanted: { false },
            completion: { _ in completionFired = true }
        )

        // The skip path is fully synchronous (no probe, no `await` before
        // the `isStillWanted` check), so by the time this call returns the
        // decision has already been made.
        XCTAssertFalse(completionFired, "addAccount must never run once the attempt was cancelled")
        XCTAssertTrue(model.accounts.isEmpty)
        XCTAssertNil(model.storedConfig(for: accountId))
    }
}
