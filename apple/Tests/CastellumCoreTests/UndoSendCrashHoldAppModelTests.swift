// Castellum - AppModel-level regression test for the S25 undo-send
// crash-safety wiring: `reconcile_undo_sends` must actually run against the
// REAL account list `AppModel.bootstrap()` loads, not a snapshot of
// `accounts` taken before `reloadAccounts()`'s async load has populated it.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

/// A first attempt at S25 called the crash-safety reconcile as a second,
/// separate step right after kicking off `reloadAccounts()` — but `accounts`
/// is filled ASYNCHRONOUSLY, off-actor, inside that same call's `load(...)`
/// completion, so at the moment the reconcile read it, it was always `[]`.
/// The hold silently never ran, on any launch, for any account. This test
/// drives the actual code path (`reloadAccounts()`, which is exactly what
/// `bootstrap()` calls) against a SECOND `AppModel` opened on the same
/// profile directory — simulating a relaunch — and proves a delayed send
/// left `queued` by an "interrupted" first session ends up `held`, not
/// silently drained.
///
/// Requires local GreenMail (see `scripts/local-imap`) to create a real
/// account row (only `sync` inserts one); skips — not fails — when it's
/// unreachable, same idiom as `AccountReconnectTests`.
@MainActor
final class UndoSendCrashHoldAppModelTests: XCTestCase {
    private static let host = "127.0.0.1"
    private static let port: UInt16 = 3143

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

    private func config(accountId: String) -> String {
        """
        {"account_id":"\(accountId)","email":"test@local.test","host":"\(Self.host)",
         "port":\(Self.port),"username":"test@local.test","password":"secret",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
    }

    /// Polls a completion-driven result to a Bool flag; fails explicitly on
    /// timeout (same idiom as `AccountReconnectTests.waitUntil`).
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

    func testReloadAccountsHoldsAnInterruptedDelayedSendOnRelaunch() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let dir = TestScratch.root
            .appendingPathComponent("castellum-crash-hold-\(UUID().uuidString)")
        let accountId = "crash-hold-\(UUID().uuidString)"

        // "First session": add the account, enable writes, save a draft and
        // queue it with a long undo window — then stop, exactly as if the
        // app had crashed before the window closed. Deliberately never
        // drains (no `sendPending` call), so `last_clean_drain_epoch` stays
        // unset and the window's continuity can't be proven.
        let firstSession = makeModel(dir: dir)
        var addResult: AccountSyncStatus?
        firstSession.addAccount(
            accountID: accountId, configJSON: config(accountId: accountId),
            completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "initial add never completed")
        XCTAssertEqual(addResult?.state, .ok)

        try firstSession.core.setAccountWriteEnabled(accountId: accountId, enabled: true)

        let draft = NewDraft(
            accountId: accountId,
            to: [DraftAddress(name: nil, email: "someone@example.com")],
            subject: "Crash-hold regression", bodyText: "test")
        var draftId: Int64?
        firstSession.saveDraft(draft) { draftId = $0 }
        try await waitUntil({ draftId != nil }, "draft save never completed")

        try firstSession.core.sendDraft(id: draftId!, delaySeconds: 3600)
        let queuedOp = try firstSession.core.pendingOperations(accountId: accountId)
            .first { $0.kind == "send" }
        XCTAssertEqual(queuedOp?.status, "queued", "sanity check: not held before any reconcile runs")

        // "Relaunch": a brand-new AppModel opened on the SAME profile
        // directory — this is the exact instance whose `reloadAccounts()`
        // must reconcile the interrupted send before anything could drain it.
        let relaunch = makeModel(dir: dir)
        relaunch.reloadAccounts()

        try await waitUntil({
            (try? relaunch.core.pendingOperations(accountId: accountId))?
                .first { $0.kind == "send" }?.status == "held"
        }, "the interrupted delayed send was never reconciled to 'held' — " +
            "reloadAccounts() never called reconcileUndoSends for a real account")

        let heldOp = try relaunch.core.pendingOperations(accountId: accountId)
            .first { $0.kind == "send" }
        XCTAssertEqual(heldOp?.status, "held")
        XCTAssertEqual(heldOp?.notBeforeEpoch, queuedOp?.notBeforeEpoch)
    }
}
