// Castellum - M6: the contacts refresh runs once per launch, AFTER the first
// list reaches the UI, off the main actor, and never re-entrantly.
// The refresh is replaced by a spy (`AppModel.contactsRefreshWork`), so no
// test here recomputes real aggregates; each model has its own temp profile.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

/// Thread-safe log shared by the spy (runs on a background thread) and the test.
private final class RefreshSpy: @unchecked Sendable {
    private let lock = NSLock()
    private var _starts = 0, _running = 0, _maxRunning = 0, _onMain = false
    private var _blocking = false
    let gate = DispatchSemaphore(value: 0)

    var starts: Int { lock.withLock { _starts } }
    var maxRunning: Int { lock.withLock { _maxRunning } }
    var onMainThread: Bool { lock.withLock { _onMain } }
    var blocking: Bool {
        get { lock.withLock { _blocking } }
        set { lock.withLock { _blocking = newValue } }
    }

    func run() {
        let block = lock.withLock { () -> Bool in
            _starts += 1; _running += 1; _maxRunning = max(_maxRunning, _running)
            if Thread.isMainThread { _onMain = true }
            return _blocking
        }
        if block { gate.wait() }
        lock.withLock { _running -= 1 }
    }
}

@MainActor
final class LaunchContactsRefreshTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel(_ spy: RefreshSpy) -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-m6-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        model.contactsRefreshWork = { _ in spy.run() }
        // Keep the fallback timer out of the ordering assertions.
        model.launchContactsFallbackNanos = 600_000_000_000
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(_ what: String, timeout: TimeInterval = 5,
                           _ cond: @escaping () -> Bool) async {
        let deadline = Date().addingTimeInterval(timeout)
        while !cond(), Date() < deadline { try? await Task.sleep(nanoseconds: 10_000_000) }
        XCTAssertTrue(cond(), "timed out waiting for \(what)")
    }

    /// Armed by the REAL `bootstrap()` (so removing the arm call there turns
    /// this red), the refresh must NOT start until the first list has been
    /// delivered, then run exactly once, off the main thread.
    func testRefreshStartsOnlyAfterFirstListThenRunsOnce() async {
        let spy = RefreshSpy()
        let model = makeModel(spy)
        model.bootstrap()
        // Let anything wrongly started at launch get a chance to run.
        try? await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(spy.starts, 0, "refresh started before the first list was delivered")

        model.select(unifiedRole: "inbox")           // the first list
        await waitUntil("first list delivered") { model.messagesGeneration > 0 }
        await waitUntil("refresh started") { spy.starts == 1 }

        // Later list deliveries and a second arm never start another launch refresh.
        model.select(unifiedRole: "inbox")
        model.armLaunchContactsRefresh()
        model.startLaunchContactsRefreshIfArmed()
        try? await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(spy.starts, 1, "launch refresh must run exactly once")
        XCTAssertFalse(spy.onMainThread, "refresh must run off the main actor")
    }

    /// A trigger while a refresh is running does not start a second one; it
    /// only queues a single follow-up that runs after the first finishes.
    func testSecondTriggerWhileRunningDoesNotStartAnother() async {
        let spy = RefreshSpy()
        spy.blocking = true
        let model = makeModel(spy)
        model.refreshContactsNow()
        await waitUntil("first refresh running") { spy.starts == 1 }

        model.refreshContactsNow()
        model.refreshContactsNow()
        model.armLaunchContactsRefresh()
        model.startLaunchContactsRefreshIfArmed()
        try? await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(spy.starts, 1, "a refresh started while one was running")

        spy.blocking = false
        spy.gate.signal()                              // release the first
        await waitUntil("single follow-up") { spy.starts == 2 }
        try? await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(spy.starts, 2, "triggers during a run collapse into ONE follow-up")
        XCTAssertEqual(spy.maxRunning, 1, "two refreshes overlapped")
    }

    /// A launch with no list at all (no accounts, nothing selected) still gets
    /// its refresh, via the fallback timer.
    func testFallbackStartsRefreshWhenNoListIsEverDelivered() async {
        let spy = RefreshSpy()
        let model = makeModel(spy)
        model.launchContactsFallbackNanos = 50_000_000
        model.bootstrap()
        await waitUntil("fallback refresh") { spy.starts == 1 }
    }

    /// Opening the Contacts window before the launch refresh ran refreshes
    /// then, and consumes the launch refresh (no duplicate afterwards).
    func testOpeningContactsEarlyRefreshesAndConsumesLaunchRefresh() async {
        let spy = RefreshSpy()
        let model = makeModel(spy)
        model.bootstrap()
        model.refreshContactsNow()                     // what openContacts() calls
        await waitUntil("early refresh") { spy.starts == 1 }
        model.select(unifiedRole: "inbox")
        await waitUntil("list delivered") { model.messagesGeneration > 0 }
        try? await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(spy.starts, 1)
    }

    // MARK: - X1: the one-off encoded-text repair rides the launch refresh

    /// The repair must not start before the first list is on screen, must run
    /// exactly once per launch, off the main thread, and BEFORE the contacts
    /// refresh (which derives names from the rows it rewrites).
    func testEncodedTextRepairRunsOnceAfterFirstListAndBeforeContacts() async {
        let order = OrderLog()
        let spy = RefreshSpy()
        let model = makeModel(spy)
        model.envelopeRepairWork = { _ in
            order.add(Thread.isMainThread ? "repair-on-main" : "repair")
            return 0
        }
        model.contactsRefreshWork = { _ in order.add("contacts"); spy.run() }
        model.bootstrap()
        try? await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(order.entries, [], "repair started before the first list was delivered")

        model.select(unifiedRole: "inbox")           // the first list
        await waitUntil("contacts refresh after the repair") { spy.starts == 1 }
        XCTAssertEqual(order.entries, ["repair", "contacts"])

        model.select(unifiedRole: "inbox")
        model.armLaunchContactsRefresh()
        model.startLaunchContactsRefreshIfArmed()
        model.refreshContactsNow()                    // a later, ordinary refresh
        await waitUntil("second contacts refresh") { spy.starts == 2 }
        XCTAssertEqual(order.entries, ["repair", "contacts", "contacts"],
                       "the repair runs once per launch, not once per refresh")
    }

    /// A failing repair is background maintenance: no alert, and the contacts
    /// refresh still runs.
    func testEncodedTextRepairFailureIsSilentAndContactsStillRefresh() async {
        struct Boom: Error {}
        let spy = RefreshSpy()
        let model = makeModel(spy)
        model.envelopeRepairWork = { _ in throw Boom() }
        model.bootstrap()
        model.select(unifiedRole: "inbox")
        await waitUntil("contacts refresh despite the failure") { spy.starts == 1 }
        XCTAssertNil(model.errorText)
    }

    /// The real core call on a fresh temp profile: the report decodes, and the
    /// repair is one-shot (the second call reads nothing).
    func testEncodedTextRepairReportDecodesAndIsOneShot() throws {
        let model = makeModel(RefreshSpy())
        let first = try model.core.repairEncodedEnvelopeText()
        XCTAssertFalse(first.alreadyDone)
        XCTAssertEqual(first.messagesRepaired, 0)
        let second = try model.core.repairEncodedEnvelopeText()
        XCTAssertTrue(second.alreadyDone)
        XCTAssertEqual(second.batches, 0)
    }
}

/// Thread-safe ordered log for work that runs off the main thread.
private final class OrderLog: @unchecked Sendable {
    private let lock = NSLock()
    private var _entries: [String] = []
    var entries: [String] { lock.withLock { _entries } }
    func add(_ entry: String) { lock.withLock { _entries.append(entry) } }
}
