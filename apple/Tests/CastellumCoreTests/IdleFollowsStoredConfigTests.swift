// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// S31b review of PR #172 (IMAP IDLE push), findings (c) and (e): IDLE must
/// only ever run on the config the Keychain holds and the folders the user
/// still syncs.
///
/// `idleStartOverride` records every IDLE (re)start instead of opening a
/// connection, so the tests can assert WHICH config IDLE was started with. No
/// real mail server (a loopback `StallingImapServer`), no real Keychain
/// (TestHost routes `AccountStore` to the in-memory backing).
@MainActor
final class IdleFollowsStoredConfigTests: XCTestCase {
    private var models: [AppModel] = []
    private var servers: [StallingImapServer] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-idle-config-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

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

    private func config(
        accountId: String, user: String, port: UInt16, password: String, mailboxes: String = "[\"INBOX\"]"
    ) -> String {
        """
        {"account_id":"\(accountId)","email":"\(user)","host":"127.0.0.1",
         "port":\(port),"username":"\(user)","password":"\(password)",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":\(mailboxes)}
        """
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

    /// Finding (c): `performSync`'s `onSuccess` also runs for a superseded
    /// attempt, and it closes over THAT attempt's config. Attempt 1 (an old
    /// password) is held mid-sync; attempt 2 (the Reconnect, a new password)
    /// supersedes it. When attempt 1 finishes late it must not start IDLE on
    /// its stale config — only attempt 2 may, and with its own.
    func testASupersededAttemptDoesNotStartIdleOnItsStaleConfig() async throws {
        let server = try StallingImapServer()
        servers.append(server)
        let model = makeModel()
        let user = "idle-gen@mock.test"
        let id = "idle-gen-\(UUID().uuidString)"
        let stale = config(accountId: id, user: user, port: server.port, password: "old-password")
        let fresh = config(accountId: id, user: user, port: server.port, password: "new-password")
        var idleStarts: [String] = []
        model.idleStartOverride = { _, json in idleStarts.append(json) }
        server.hold(user: user)

        model.addAccount(accountID: id, configJSON: stale)
        try await waitUntil(
            { server.isStalled(user: user) && model.accounts.contains { $0.id == id } },
            "attempt 1 never reached the held descriptor fetch")

        var result: AccountSyncStatus?
        model.addAccount(accountID: id, configJSON: fresh, completion: { result = $0 })
        // Attempt 2 connects but queues behind attempt 1 in the core (one pass
        // per account at a time); give it a moment to be in flight.
        try await Task.sleep(nanoseconds: 600_000_000)
        XCTAssertTrue(idleStarts.isEmpty, "nothing has completed yet")

        server.release(user: user)
        try await waitUntil({ result?.state == .ok }, "attempt 2 never completed")
        // Let any (wrongly) late start from attempt 1 land before asserting.
        try await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(
            idleStarts, [fresh],
            "IDLE must start once, on the latest attempt's config — never on the superseded one")
        XCTAssertEqual(model.storedConfig(for: id), fresh)
    }

    /// Finding (e): IDLE watches INBOX only, and INBOX can be unticked in
    /// Settings > Accounts > Folders…. Saving the selection must restart IDLE
    /// with the NEW config (the core then runs no IDLE for an account whose
    /// folders exclude INBOX) — before the purge, so a wake cannot re-import
    /// the INBOX the purge removes.
    func testSavingAFolderSelectionRestartsIdleWithTheNewConfig() async throws {
        let model = makeModel()
        let id = "idle-folders-\(UUID().uuidString)"
        let withoutInbox = config(
            accountId: id, user: "idle-folders@mock.test", port: 1, password: "pw",
            mailboxes: "[\"Sent\"]")
        var idleStarts: [String] = []
        model.idleStartOverride = { _, json in idleStarts.append(json) }

        var outcome: AppModel.FolderSelectionUpdateResult?
        model.updateFolderSelection(
            accountId: id, configJSON: withoutInbox, keepPaths: ["Sent"]) { outcome = $0 }
        try await waitUntil({ outcome != nil }, "updateFolderSelection never completed")
        XCTAssertEqual(
            idleStarts, [withoutInbox],
            "the folder edit must restart IDLE with the new (INBOX-less) config")
    }

    /// Audit #3439: Re-authenticate then Save must restart IDLE with the new
    /// password, not leave the old connection running; a failed save must not.
    func testReauthenticateSaveRestartsIdleWithTheNewPassword() async throws {
        let model = makeModel()
        let id = "idle-reauth-\(UUID().uuidString)"
        let fresh = config(accountId: id, user: "idle-reauth@mock.test", port: 1, password: "new-password")
        var idleStarts: [String] = []
        model.idleStartOverride = { _, json in idleStarts.append(json) }
        XCTAssertTrue(model.saveAccountWithoutSync(accountID: id, configJSON: fresh))
        XCTAssertEqual(idleStarts, [fresh])
        idleStarts = []
        XCTAssertFalse(model.saveAccountWithoutSync(accountID: id, configJSON: "not json"))
        XCTAssertTrue(idleStarts.isEmpty, "a failed save must not restart IDLE")
    }
}
