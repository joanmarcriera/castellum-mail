// Castellum - opening a clicked Spotlight result (, slices 1-2).
// `AppDelegate.application(_:continue:)` / `.onContinueUserActivity` hand the
// indexed message id to the model; a cold launch queues it until accounts have
// loaded; a result whose message is gone shows a notice and is deleted from the
// index. `activateMainWindow` and `openSpotlightResult` are stubbed so no test
// touches real `NSApp` state, like AppDelegateMailtoFlushTests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import CoreSpotlight
import XCTest
@testable import Castellum

@MainActor
final class SpotlightContinuationTests: XCTestCase {
    private var models: [AppModel] = []
    private let spotlightIndex = MockSpotlightIndex()

    private func makeModel() -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-spotlight-open-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir, spotlightIndex: spotlightIndex)
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

    private func waitUntilAccountsLoaded(_ model: AppModel) async throws {
        let deadline = Date().addingTimeInterval(15)
        while !model.accountsLoaded && Date() < deadline { try await Task.sleep(nanoseconds: 50_000_000) }
        XCTAssertTrue(model.accountsLoaded, "timed out waiting for accountsLoaded")
    }

    private func activity(_ type: String = CSSearchableItemActionType, id: Any? = "acc:INBOX:7") -> NSUserActivity {
        let a = NSUserActivity(activityType: type)
        if let id { a.userInfo = [CSSearchableItemActivityIdentifier: id] }
        return a
    }

    // MARK: pure id extraction

    func testActivityWithSearchableItemTypeYieldsTheId() {
        XCTAssertEqual(AppDelegate.spotlightMessageId(from: activity()), "acc:INBOX:7")
    }

    func testOtherActivityTypeIsIgnoredAndReturnsFalse() {
        XCTAssertNil(AppDelegate.spotlightMessageId(from: activity("com.apple.handoff")))
        let delegate = AppDelegate()
        XCTAssertFalse(delegate.application(NSApplication.shared, continue: activity("other"), restorationHandler: { _ in }))
        XCTAssertTrue(delegate.application(NSApplication.shared, continue: activity(), restorationHandler: { _ in }))
    }

    func testEmptyOversizedOrNulIdIsRejected() {
        XCTAssertNil(AppDelegate.spotlightMessageId(from: activity(id: "")))
        XCTAssertNil(AppDelegate.spotlightMessageId(from: activity(id: String(repeating: "x", count: 1025))))
        XCTAssertNil(AppDelegate.spotlightMessageId(from: activity(id: "acc:INBOX\u{0}:7")))
        XCTAssertNil(AppDelegate.spotlightMessageId(from: activity(id: 42)))
        XCTAssertNil(AppDelegate.spotlightMessageId(from: activity(id: nil)))
        XCTAssertEqual(
            AppDelegate.spotlightMessageId(from: activity(id: String(repeating: "x", count: 1024)))?.utf8.count, 1024)
    }

    // MARK: cold and warm delivery

    func testContinuationBeforeModelExistsIsDeliveredOnceAfterAccountsLoad() async throws {
        let model = makeModel()
        let delegate = AppDelegate()
        var activated = 0
        delegate.activateMainWindow = { activated += 1 }
        var opened: [String] = []
        delegate.openSpotlightResult = { id, _ in opened.append(id) }

        delegate.handleSpotlightContinuation(messageId: "acc:INBOX:7")
        XCTAssertTrue(opened.isEmpty, "no model yet")
        delegate.modelBecameAvailable(model)
        XCTAssertEqual(activated, 1, "a cold Spotlight launch fronts the window")
        XCTAssertTrue(opened.isEmpty, "accounts not loaded yet")
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        XCTAssertEqual(opened, ["acc:INBOX:7"], "delivered exactly once")
        XCTAssertEqual(activated, 1, "not fronted a second time")
    }

    func testSecondPendingIdReplacesTheFirst() async throws {
        let model = makeModel()
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        var opened: [String] = []
        delegate.openSpotlightResult = { id, _ in opened.append(id) }
        delegate.handleSpotlightContinuation(messageId: "first")
        delegate.handleSpotlightContinuation(messageId: "second")
        delegate.modelBecameAvailable(model)
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        XCTAssertEqual(opened, ["second"])
    }

    func testContinuationAfterAccountsLoadedDeliversImmediatelyAndFrontsTheWindow() async throws {
        let model = makeModel()
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        let delegate = AppDelegate()
        var activated = 0
        delegate.activateMainWindow = { activated += 1 }
        var opened: [String] = []
        delegate.openSpotlightResult = { id, _ in opened.append(id) }
        delegate.modelBecameAvailable(model)
        XCTAssertEqual(activated, 0, "an ordinary launch never fronts the window")
        delegate.handleSpotlightContinuation(messageId: "acc:INBOX:9")
        XCTAssertEqual(opened, ["acc:INBOX:9"])
        XCTAssertEqual(activated, 1)
    }

    func testDuplicateDeliveryWithinTwoSecondsIsDroppedButALaterOneOpens() async throws {
        let model = makeModel()
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        var now = Date(timeIntervalSince1970: 1_000)
        delegate.spotlightClock = { now }
        var opened: [String] = []
        delegate.openSpotlightResult = { id, _ in opened.append(id) }
        delegate.modelBecameAvailable(model)
        delegate.handleSpotlightContinuation(messageId: "x")
        now = now.addingTimeInterval(0.5)
        delegate.handleSpotlightContinuation(messageId: "x")      // both entry points firing
        XCTAssertEqual(opened, ["x"])
        now = now.addingTimeInterval(3)
        delegate.handleSpotlightContinuation(messageId: "x")      // the user clicked again
        XCTAssertEqual(opened, ["x", "x"])
    }

    func testAnInvalidIdIsNeverQueuedOrDelivered() async throws {
        let model = makeModel()
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        var opened: [String] = []
        delegate.openSpotlightResult = { id, _ in opened.append(id) }
        delegate.handleSpotlightContinuation(messageId: "")
        delegate.handleSpotlightContinuation(messageId: "a\u{0}b")
        delegate.modelBecameAvailable(model)
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        XCTAssertTrue(opened.isEmpty)
    }

    // MARK: model: not found

    func testUnknownIdReportsNotFoundSetsTheNoticeAndDeletesThatIndexItem() async throws {
        let model = makeModel()
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        var outcome: MessageRevealOutcome?
        model.revealMessage(id: "nope:INBOX:999999") { outcome = $0 }
        let deadline = Date().addingTimeInterval(10)
        while outcome == nil && Date() < deadline { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertEqual(outcome, .notFound)

        model.openSpotlightResult(messageId: "nope:INBOX:999999")
        let deadline2 = Date().addingTimeInterval(10)
        while spotlightIndex.deletedIdentifierBatches.isEmpty && Date() < deadline2 {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertNotNil(model.smartMailboxNotice)
        XCTAssertEqual(spotlightIndex.deletedIdentifierBatches, [["nope:INBOX:999999"]])
        XCTAssertTrue(spotlightIndex.deletedDomainBatches.isEmpty, "never a domain delete")
        XCTAssertFalse(spotlightIndex.sawMainThreadCall, "index calls stay off the main thread")
    }

    func testAFoundMessageDeletesNothingAndClearsTheNotice() async throws {
        let model = makeModel()
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        let dir = model.profileDirectory
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: "acc-sp", email: "me@x.test",
            mailboxId: "acc-sp:INBOX", role: "inbox")
        try DirectFixture.seedMessage(
            profileDirectory: dir, id: "acc-sp:INBOX:1", accountId: "acc-sp", mailboxId: "acc-sp:INBOX",
            subject: "Findable", fromAddress: "a@x.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        model.smartMailboxNotice = "stale"
        var outcome: MessageRevealOutcome?
        model.revealMessage(id: "acc-sp:INBOX:1") { outcome = $0 }
        let deadline = Date().addingTimeInterval(10)
        while outcome == nil && Date() < deadline { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertEqual(outcome, .shown)
        XCTAssertNil(model.smartMailboxNotice)
        XCTAssertTrue(spotlightIndex.deletedIdentifierBatches.isEmpty)
    }

    // MARK: indexer

    func testRemoveIdsDeletesExactlyThoseIdentifiersOffTheMainThread() async {
        let indexer = SpotlightIndexer(index: spotlightIndex)
        await indexer.remove(ids: ["a:1", "b:2"])
        await indexer.remove(ids: [])
        XCTAssertEqual(spotlightIndex.deletedIdentifierBatches, [["a:1", "b:2"]])
        XCTAssertTrue(spotlightIndex.deletedDomainBatches.isEmpty)
    }
}
