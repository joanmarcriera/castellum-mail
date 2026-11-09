// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// The Focus filter's state, store and intent (S59). The store tests use a
/// private UserDefaults suite; the one `perform()` test writes the standard
/// domain and restores `.inactive` afterwards.
final class FocusFilterStateTests: XCTestCase {
    private func suite() -> UserDefaults {
        let name = "castellum-focus-\(UUID().uuidString)"
        addTeardownBlock { UserDefaults(suiteName: name)?.removePersistentDomain(forName: name) }
        return UserDefaults(suiteName: name)!
    }

    func testStoreRoundTripsAndDefaultsToInactive() {
        let d = suite()
        XCTAssertEqual(FocusFilterStore.load(defaults: d), .inactive)
        let state = FocusFilterState(allowedAccountIds: ["a", "b"], vipsOnly: true, showDockBadge: false)
        FocusFilterStore.save(state, defaults: d)
        XCTAssertEqual(FocusFilterStore.load(defaults: d), state)
    }

    func testIntentWithDefaultParametersDescribesTheInactiveState() {
        let intent = CastellumFocusFilter()
        XCTAssertEqual(intent.state, .inactive)
    }

    func testIntentWithTwoAccountsAndVipsOnlyDescribesExactlyThat() {
        var intent = CastellumFocusFilter()
        intent.accounts = [AccountEntity(id: "a", label: "A"), AccountEntity(id: "b", label: "B")]
        intent.vipsOnly = true
        intent.showDockBadge = false
        XCTAssertEqual(
            intent.state,
            FocusFilterState(allowedAccountIds: ["a", "b"], vipsOnly: true, showDockBadge: false))
    }

    func testPerformSavesTheStateAndPostsTheChangeNotification() async throws {
        var intent = CastellumFocusFilter()
        intent.accounts = [AccountEntity(id: "a", label: "A")]
        intent.vipsOnly = true
        let posted = expectation(forNotification: .castellumFocusFilterChanged, object: nil)
        _ = try await intent.perform()
        await fulfillment(of: [posted], timeout: 5)
        XCTAssertEqual(
            FocusFilterStore.load(),
            FocusFilterState(allowedAccountIds: ["a"], vipsOnly: true, showDockBadge: true))
        // Leave the test host's defaults as we found them.
        FocusFilterStore.save(.inactive)
    }

    func testAccountEntityQueryFiltersTheSnapshot() async throws {
        FocusAccountsSnapshot.save([("a", "a@x.test"), ("b", "b@x.test")])
        addTeardownBlock { FocusAccountsSnapshot.save([]) }
        let query = AccountEntityQuery()
        let hit = try await query.entities(for: ["b"])
        XCTAssertEqual(hit.map(\.id), ["b"])
        let all = try await query.suggestedEntities()
        XCTAssertEqual(all.map(\.label), ["a@x.test", "b@x.test"])
    }
}
