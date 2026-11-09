// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Uses an isolated per-test Keychain service namespace so it never touches real
/// stored accounts, and tears the namespace down after each test.
final class AccountStoreTests: XCTestCase {
    private var service = ""
    private var store: AccountStore!

    override func setUp() {
        super.setUp()
        service = "com.castellum.test.\(UUID().uuidString)"
        store = AccountStore(service: service)
    }

    override func tearDown() {
        for id in (try? store.allAccountIDs()) ?? [] {
            try? store.delete(accountID: id)
        }
        super.tearDown()
    }

    func testRoundTrip() throws {
        XCTAssertNil(try store.configJSON(for: "a@x.test"))
        try store.save(accountID: "a@x.test", configJSON: #"{"host":"imap.x.test","password":"s3cret"}"#)
        XCTAssertEqual(try store.configJSON(for: "a@x.test"),
                       #"{"host":"imap.x.test","password":"s3cret"}"#)
    }

    func testSaveReplacesExisting() throws {
        try store.save(accountID: "a@x.test", configJSON: #"{"password":"old"}"#)
        try store.save(accountID: "a@x.test", configJSON: #"{"password":"new"}"#)
        XCTAssertEqual(try store.configJSON(for: "a@x.test"), #"{"password":"new"}"#)
        // Still exactly one item for that account.
        XCTAssertEqual(try store.allAccountIDs().filter { $0 == "a@x.test" }.count, 1)
    }

    func testAllAccountIDsAndDelete() throws {
        try store.save(accountID: "a@x.test", configJSON: "{}")
        try store.save(accountID: "b@x.test", configJSON: "{}")
        XCTAssertEqual(Set(try store.allAccountIDs()), ["a@x.test", "b@x.test"])
        try store.delete(accountID: "a@x.test")
        XCTAssertEqual(try store.allAccountIDs(), ["b@x.test"])
        XCTAssertNil(try store.configJSON(for: "a@x.test"))
    }

    /// The read cache must never outlive the Keychain item (a removed account
    /// would otherwise keep syncing with stale credentials until relaunch) and
    /// must not hide a value written through a different store instance.
    func testCacheFollowsKeychain() throws {
        try store.save(accountID: "a@x.test", configJSON: #"{"password":"one"}"#)
        XCTAssertEqual(try store.configJSON(for: "a@x.test"), #"{"password":"one"}"#)
        try store.delete(accountID: "a@x.test")
        XCTAssertNil(try store.configJSON(for: "a@x.test"))

        let other = AccountStore(service: service)
        try other.save(accountID: "b@x.test", configJSON: #"{"password":"two"}"#)
        XCTAssertEqual(try store.configJSON(for: "b@x.test"), #"{"password":"two"}"#)
    }

    func testDeleteMissingIsNotAnError() throws {
        XCTAssertNoThrow(try store.delete(accountID: "ghost@x.test"))
    }

    /// Only the real profile may use the shipped namespace; every other
    /// profile directory gets a stable namespace of its own.
    func testServiceIsScopedToTheProfileDirectory() {
        XCTAssertEqual(AccountStore.service(forProfileDirectory: nil), AccountStore.defaultService)
        let a = URL(fileURLWithPath: "/tmp/castellum-a", isDirectory: true)
        let b = URL(fileURLWithPath: "/tmp/castellum-b", isDirectory: true)
        let serviceA = AccountStore.service(forProfileDirectory: a)
        XCTAssertNotEqual(serviceA, AccountStore.defaultService)
        XCTAssertTrue(serviceA.hasPrefix(AccountStore.defaultService + ".profile."))
        XCTAssertEqual(serviceA, AccountStore.service(forProfileDirectory: a))
        XCTAssertEqual(
            serviceA,
            AccountStore.service(forProfileDirectory: URL(fileURLWithPath: "/tmp/./castellum-a/")))
        XCTAssertNotEqual(serviceA, AccountStore.service(forProfileDirectory: b))
    }

    func testDeleteAllEmptiesOnlyThisNamespace() throws {
        let other = AccountStore(service: service + ".other")
        defer { try? other.deleteAll() }
        try store.save(accountID: "a@x.test", configJSON: "{}")
        try store.save(accountID: "b@x.test", configJSON: "{}")
        try other.save(accountID: "c@x.test", configJSON: "{}")
        try store.deleteAll()
        XCTAssertEqual(try store.allAccountIDs(), [])
        XCTAssertEqual(try other.allAccountIDs(), ["c@x.test"])
    }

    /// The 0.3.5 prompt storm: a test's temp-profile AppModel must never
    /// read or write the shipped app's Keychain namespace.
    @MainActor
    func testAppModelWithACustomProfileNeverUsesTheDefaultNamespace() throws {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-ns-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
        XCTAssertEqual(
            model.accountStore.serviceName, AccountStore.service(forProfileDirectory: dir))
    }

    /// The test host must recognise itself, or `xcodebuild test` opens and
    /// syncs the developer's real profile (2026-09-17).
    func testTestHostGuardIsActiveUnderXCTest() {
        XCTAssertTrue(TestHost.isActive)
        let dir = try? XCTUnwrap(TestHost.profileDirectory)
        XCTAssertNotNil(dir)
        XCTAssertTrue(dir?.lastPathComponent.hasPrefix("castellum-testhost-") ?? false)
        XCTAssertFalse(TestHost.isTestHost(environment: [:]))
        XCTAssertFalse(TestHost.isTestHost(environment: ["XCTestBundlePath": ""]))
        XCTAssertTrue(TestHost.isTestHost(environment: ["XCTestConfigurationFilePath": "/x"]))
    }
}
