// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// A FAILED sync must clear `isSyncing`: the toolbar spinner is
/// bound to it, so a leaked `beginSync()` without a matching `endSync()` on the
/// error path leaves the spinner spinning forever (e.g. launch re-sync against
/// an unreachable host). Same invariant for `addAccount`, which shares the
/// pattern.
@MainActor
final class SyncSpinnerTests: XCTestCase {

    /// Nothing listens on 127.0.0.1:1, so `syncAccount` fails fast with a
    /// connection error — exercising the load() error path deterministically.
    private static let unreachableConfig = """
    {"account_id":"spinner-test","email":"t@local.test","host":"127.0.0.1","port":1,
     "username":"t@local.test","password":"x","use_tls":false,
     "allow_invalid_certs":false,"mailboxes":["INBOX"]}
    """

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-spinner-\(UUID().uuidString)")
        return AppModel(profileDirectory: dir)
    }

    /// Poll (yielding the main actor so MainActor.run hops can land) until
    /// `isSyncing` clears; fails explicitly on timeout so a slow-CI failure is
    /// unambiguous rather than surfacing as a generic downstream assertion.
    private func waitUntilNotSyncing(
        _ model: AppModel, timeout: TimeInterval = 15
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while model.isSyncing && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if model.isSyncing {
            XCTFail("timed out (\(timeout)s) waiting for isSyncing to clear")
        }
    }

    func testFailedSyncClearsIsSyncing() async throws {
        let model = makeModel()
        XCTAssertFalse(model.isSyncing)

        model.sync(config: Self.unreachableConfig)
        XCTAssertTrue(model.isSyncing, "spinner must show while the sync is in flight")

        try await waitUntilNotSyncing(model)
        XCTAssertFalse(
            model.isSyncing,
            "failed sync leaked isSyncing — toolbar spinner would spin forever")
        XCTAssertNil(model.errorText, "background sync failures must not open the modal alert")
        XCTAssertEqual(model.accountSyncStatuses["spinner-test"]?.state, .unreachable)
        XCTAssertNotNil(model.syncBanner, "background sync failure should use the banner slot")
    }

    func testFailedAddAccountClearsIsSyncing() async throws {
        let model = makeModel()
        XCTAssertFalse(model.isSyncing)

        model.addAccount(accountID: "spinner-test", configJSON: Self.unreachableConfig)
        XCTAssertTrue(model.isSyncing, "spinner must show while the first sync is in flight")

        try await waitUntilNotSyncing(model)
        XCTAssertFalse(
            model.isSyncing,
            "failed addAccount leaked isSyncing — toolbar spinner would spin forever")
        XCTAssertNil(model.errorText, "account connection failures must not open the modal alert")
        XCTAssertEqual(model.accountSyncStatuses["spinner-test"]?.state, .unreachable)
    }
}
