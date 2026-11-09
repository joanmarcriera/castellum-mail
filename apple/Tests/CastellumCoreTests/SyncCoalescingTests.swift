// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// S72 (, Marc's 2026-09-26 sample): EIGHT
/// `castellum_sync_account` calls were in flight for FOUR accounts — the
/// periodic scheduler, the launch `reSyncAll`, replay-triggered re-syncs and
/// Retry all call `sync(config:)`, and nothing stopped one of them starting a
/// second full pass on an account whose (21k-message) first sync was still
/// running. A resumed first sync then took 12 minutes instead of ~50 s.
///
/// The rule: while an account has a sync in flight, further `sync(config:)`
/// requests for it are coalesced into ONE queued re-run, started when the
/// in-flight pass finishes. The held sync is a scripted loopback IMAP server
/// (`StallingImapServer`) stalled after LOGIN/LIST/SELECT, so "in flight" is
/// a fact the test controls. No GreenMail, no real Keychain.
@MainActor
final class SyncCoalescingTests: XCTestCase {
    private var models: [AppModel] = []
    private var servers: [StallingImapServer] = []

    override func tearDown() async throws {
        for server in servers { server.stop() }
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

    func testSyncRequestsForAnInFlightAccountCoalesceIntoOneQueuedRerun() async throws {
        let server = try StallingImapServer()
        servers.append(server)
        let dir = TestScratch.root
            .appendingPathComponent("castellum-coalesce-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        let user = "coalesce@mock.test"
        let id = "coalesce-\(UUID().uuidString)"
        let cfg = config(accountId: id, user: user, port: server.port)
        server.hold(user: user)

        model.sync(config: cfg)
        try await waitUntil({ server.isStalled(user: user) },
                            "the first sync never reached the held descriptor fetch")

        // A periodic tick, a replay-triggered re-sync and a Retry all land
        // while the first pass is still running.
        model.sync(config: cfg)
        model.sync(config: cfg)
        model.sync(config: cfg)
        XCTAssertEqual(model.syncPassesStarted[id], 1,
                       "a second pass was stacked on an in-flight sync")

        server.release(user: user)
        try await waitUntil({ (model.syncPassesStarted[id] ?? 0) >= 2 && !model.isSyncing },
                            "the coalesced re-run never ran to completion")
        // Give a wrongly-queued third pass the chance to show up.
        try await Task.sleep(nanoseconds: 500_000_000)
        XCTAssertEqual(model.syncPassesStarted[id], 2,
                       "three requests during one pass must become exactly ONE re-run")
        XCTAssertFalse(model.isSyncing)
        XCTAssertEqual(model.accountSyncStatuses[id]?.state, .ok)
    }
}
