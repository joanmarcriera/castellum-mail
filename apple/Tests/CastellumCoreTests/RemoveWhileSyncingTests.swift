// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - removing an account while it syncs, or when the Keychain delete fails, must stick
//. Uses the scripted loopback IMAP server from SyncCoalescingTests, so "a sync is
// in flight" is a fact the test controls. No GreenMail, no real Keychain.

import XCTest
@testable import Castellum

@MainActor
final class RemoveWhileSyncingTests: XCTestCase {
    private var models: [AppModel] = []
    private var servers: [StallingImapServer] = []

    override func tearDown() async throws {
        for server in servers { server.stop() }
        for model in models {
            let deadline = Date().addingTimeInterval(10)
            while model.isSyncing && Date() < deadline { try await Task.sleep(nanoseconds: 50_000_000) }
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []; servers = []
        try await super.tearDown()
    }

    private func config(accountId: String, user: String, port: UInt16) -> String {
        """
        {"account_id":"\(accountId)","email":"\(user)","host":"127.0.0.1",
         "port":\(port),"username":"\(user)","password":"pw",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
    }

    private func waitUntil(_ condition: @escaping () -> Bool, timeout: TimeInterval = 10,
                           _ message: @autoclosure @escaping () -> String) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline { try await Task.sleep(nanoseconds: 50_000_000) }
        if !condition() { XCTFail(message()) }
    }

    private func makeModel(_ dir: URL? = nil) -> AppModel {
        let model = AppModel(profileDirectory: dir ?? TestScratch.root
            .appendingPathComponent("castellum-removing-\(UUID().uuidString)"))
        models.append(model)
        return model
    }

    /// A removed account is skipped by every sync entry point.
    func testATombstonedAccountNeverStartsASync() {
        let model = makeModel()
        model.removedAccountIds = ["gone"]
        model.sync(config: config(accountId: "gone", user: "g@mock.test", port: 1))
        XCTAssertNil(model.syncPassesStarted["gone"], "no pass may start for a removed account")
        XCTAssertEqual(model.excludingRemoved(["gone", "kept"]), ["kept"])
    }

    /// The tombstone survives a relaunch (a failed Keychain delete is retried then).
    func testTheTombstoneIsPersistedInTheProfile() {
        let dir = TestScratch.root.appendingPathComponent("castellum-tombstone-\(UUID().uuidString)")
        let first = makeModel(dir)
        first.removedAccountIds = ["a", "b"]
        let second = makeModel(dir)
        XCTAssertEqual(second.removedAccountIds, ["a", "b"])
        second.removedAccountIds = []
        XCTAssertEqual(makeModel(dir).removedAccountIds, [])
    }

    /// Remove while the first sync is running: the late completion must not write the credentials
    /// back, and the account (row and Keychain entry) must be gone once the pass ends.
    func testRemovingAnAccountMidSyncLeavesNothingBehind() async throws {
        let server = try StallingImapServer()
        servers.append(server)
        let model = makeModel()
        let user = "removing@mock.test"
        let id = "removing-\(UUID().uuidString)"
        server.hold(user: user)
        model.addAccount(accountID: id, configJSON: config(accountId: id, user: user, port: server.port))
        try await waitUntil({ server.isStalled(user: user) }, "the sync never reached the held fetch")

        model.removeAccount(accountId: id)
        XCTAssertTrue(model.removedAccountIds.contains(id), "the tombstone is set synchronously")

        server.release(user: user)
        try await waitUntil({ !model.isSyncing && !model.removedAccountIds.contains(id) },
                            "the removal never completed after the pass ended")
        XCTAssertNil(try model.accountStore.configJSON(for: id), "late save must not restore credentials")
        XCTAssertFalse(((try? model.core.listAccounts()) ?? []).contains { $0.id == id },
                       "the late pass must not leave the account row behind")
    }
}
