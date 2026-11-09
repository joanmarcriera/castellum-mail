// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

///'s explicit, opt-in coverage of the REAL Keychain code path.
///
/// Every other test in this target runs against `InMemoryKeychainBacking`
/// (the `AccountStore.init` default under `TestHost.isActive` — see
/// `KeychainBacking.swift`), so this is the only place `RealKeychainBacking`
/// — and therefore the actual `SecItemAdd`/`Update`/`CopyMatching`/`Delete`
/// calls — still run. It exists so that path keeps real coverage without
/// reintroducing the hang risk that made #2499 dangerous (a genuine
/// `SecItemDelete` block, no visible auth prompt, twice in one day, on
/// Marc's live desktop since CI runs host-mode on his actual MacBook).
///
/// Skipped by default: opt in with `CASTELLUM_RUN_REAL_KEYCHAIN_TEST=1`.
/// Uses its own throwaway `com.castellum.test.real-keychain.<uuid>`
/// namespace (never `AccountStore.defaultService` or a `.profile.*` one) and
/// a tight `executionTimeAllowance`, so if the real Keychain call ever DOES
/// block, this test fails fast with a clear timeout instead of hanging the
/// whole `xcodebuild test` run the way #2499 did.
final class AccountStoreRealKeychainTests: XCTestCase {
    override func setUp() {
        super.setUp()
        // Bounds the whole test, including a genuinely hung SecItem* call —
        // the exact failure mode #2499 hit — to a clear timeout instead of
        // hanging `xcodebuild test` indefinitely.
        executionTimeAllowance = 20
    }

    func testRoundTripAgainstTheRealKeychain() throws {
        try XCTSkipUnless(
            ProcessInfo.processInfo.environment["CASTELLUM_RUN_REAL_KEYCHAIN_TEST"] == "1",
            "opt-in only; set CASTELLUM_RUN_REAL_KEYCHAIN_TEST=1 to exercise the real Keychain")

        let service = "com.castellum.test.real-keychain.\(UUID().uuidString)"
        let store = AccountStore(service: service, keychain: RealKeychainBacking())
        defer { try? store.deleteAll() }

        XCTAssertNil(try store.configJSON(for: "a@x.test"))
        try store.save(accountID: "a@x.test", configJSON: #"{"host":"imap.x.test","password":"s3cret"}"#)
        XCTAssertEqual(try store.configJSON(for: "a@x.test"),
                       #"{"host":"imap.x.test","password":"s3cret"}"#)

        try store.save(accountID: "a@x.test", configJSON: #"{"password":"new"}"#)
        XCTAssertEqual(try store.configJSON(for: "a@x.test"), #"{"password":"new"}"#)

        try store.delete(accountID: "a@x.test")
        XCTAssertNil(try store.configJSON(for: "a@x.test"))
        XCTAssertEqual(try store.allAccountIDs(), [])
    }
}
