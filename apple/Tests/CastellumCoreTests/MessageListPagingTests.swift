// Castellum - S64 Task 4: infinite scroll for the mailbox/unified message
// list. `AppModel.loadMoreMessagesIfNeeded` fetches the next keyset page
// (castellum_list_messages_page/_by_role_page, S64 Task 3) once the table is
// scrolled near the end of what is loaded, and
// `MessageListView.Coordinator.apply` uses `insertRows` (not `reloadData`)
// for that append so scroll position and selection survive it.
//
// Fixture: 1,200 messages via `DirectFixture.seedMessage` in a loop with
// distinct, strictly increasing `sentAt` values — the plan's own wording
// (`MessageListPagingTests`, "distinct sent_at") — so the newest-first
// keyset order is unambiguous and every page boundary lands on a known row.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

/// Bounded-timeout poll for `AppModel`'s `Task.detached` work — same idiom
/// as `UnsubscribeFlowTests.waitUntil`/`UndoSendReopenTests.waitUntil`.
@MainActor
private func waitUntil(
    _ condition: @escaping () -> Bool, timeout: TimeInterval = 5,
    _ message: @autoclosure @escaping () -> String
) async throws {
    let deadline = Date().addingTimeInterval(timeout)
    while !condition() && Date() < deadline {
        try await Task.sleep(nanoseconds: 20_000_000)
    }
    if !condition() { XCTFail(message()) }
}

/// Spies on the two calls `Coordinator.apply` must choose between, so a test
/// can assert WHICH ONE ran instead of only the row count afterwards (an
/// append and a full reload can both end up with the same final row count).
private final class SpyTableView: NSTableView {
    var insertRowsCalls = 0
    var reloadDataCalls = 0

    override func insertRows(at indexes: IndexSet, withAnimation: NSTableView.AnimationOptions = []) {
        insertRowsCalls += 1
        super.insertRows(at: indexes, withAnimation: withAnimation)
    }

    override func reloadData() {
        reloadDataCalls += 1
        super.reloadData()
    }
}

@MainActor
final class MessageListPagingTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-listpaging-\(UUID().uuidString)", isDirectory: true)
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

    /// Seeds one account/mailbox with `count` messages, `sentAt` strictly
    /// increasing by id order (so the newest-first list is exactly the
    /// reverse of insertion order — deterministic page boundaries).
    private func seedMessages(model: AppModel, count: Int) throws -> (accountId: String, mailboxId: String) {
        let accountId = "paging-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId,
            email: "paging@local.test", mailboxId: mailboxId)
        for i in 0..<count {
            try DirectFixture.seedMessage(
                profileDirectory: model.profileDirectory, id: "\(accountId):m-\(i)",
                accountId: accountId, mailboxId: mailboxId,
                subject: "Message \(i)", fromAddress: "sender@local.test",
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false,
                sentAt: Int64(1_700_000_000 + i))
        }
        return (accountId, mailboxId)
    }

    private func mailbox(accountId: String, mailboxId: String) -> Mailbox {
        Mailbox(
            id: mailboxId, accountId: accountId, path: "INBOX", name: "INBOX",
            role: "inbox", delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    // MARK: - AppModel paging

    func testSelectingMailboxLoadsFirstPageAndHasMore() async throws {
        let model = makeModel()
        let (accountId, mailboxId) = try seedMessages(model: model, count: 1_200)
        model.select(mailbox: mailbox(accountId: accountId, mailboxId: mailboxId))
        try await waitUntil({ !model.messages.isEmpty }, "first page never loaded")
        XCTAssertEqual(model.messages.count, 500)
        XCTAssertTrue(model.hasMoreMessages)
        XCTAssertEqual(model.lastMessagesChange, .replaced)
    }

    func testLoadMoreAppendsTheNextPage() async throws {
        let model = makeModel()
        let (accountId, mailboxId) = try seedMessages(model: model, count: 1_200)
        model.select(mailbox: mailbox(accountId: accountId, mailboxId: mailboxId))
        try await waitUntil({ model.messages.count == 500 }, "first page never loaded")

        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        try await waitUntil({ model.messages.count == 1_000 }, "second page never appended")
        XCTAssertEqual(model.lastMessagesChange, .appended(500..<1_000))
        XCTAssertTrue(model.hasMoreMessages)

        // No duplicate/skipped rows across the page boundary: every id
        // 0...999 (the 1,000 newest, since sentAt ascends with i) present
        // exactly once.
        let expectedIds = Set((200..<1_200).map { "\(accountId):m-\($0)" })
        XCTAssertEqual(Set(model.messages.map(\.id)), expectedIds)
    }

    func testLoadMoreExhaustsTheListAtTheEnd() async throws {
        let model = makeModel()
        let (accountId, mailboxId) = try seedMessages(model: model, count: 1_200)
        model.select(mailbox: mailbox(accountId: accountId, mailboxId: mailboxId))
        try await waitUntil({ model.messages.count == 500 }, "first page never loaded")
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        try await waitUntil({ model.messages.count == 1_000 }, "second page never appended")

        model.loadMoreMessagesIfNeeded(lastVisibleRow: 950)
        try await waitUntil({ model.messages.count == 1_200 }, "third page never appended")
        XCTAssertEqual(model.lastMessagesChange, .appended(1_000..<1_200))
        XCTAssertFalse(model.hasMoreMessages, "the 1,200th message is the last one seeded")

        // A further scroll near the (now exhausted) end is a no-op.
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 1_150)
        try await Task.sleep(nanoseconds: 100_000_000)
        XCTAssertEqual(model.messages.count, 1_200)
    }

    func testALatePageAfterSwitchingMailboxIsDropped() async throws {
        let model = makeModel()
        let (accountIdA, mailboxIdA) = try seedMessages(model: model, count: 1_200)
        let accountIdB = "paging-\(UUID().uuidString)"
        let mailboxIdB = "\(accountIdB):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountIdB,
            email: "paging-b@local.test", mailboxId: mailboxIdB)
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: "\(accountIdB):only",
            accountId: accountIdB, mailboxId: mailboxIdB,
            subject: "Only message in B", fromAddress: "sender@local.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false,
            sentAt: 1_800_000_000)

        model.select(mailbox: mailbox(accountId: accountIdA, mailboxId: mailboxIdA))
        try await waitUntil({ model.messages.count == 500 }, "mailbox A's first page never loaded")

        // Kick off a page fetch for A, then IMMEDIATELY switch to B before it
        // can land — both calls run synchronously up to their `Task.detached`
        // kick-off, so B's `select` unconditionally wins the `listToken` race.
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        model.select(mailbox: mailbox(accountId: accountIdB, mailboxId: mailboxIdB))

        try await waitUntil({ model.messages.count == 1 }, "mailbox B's page never loaded")
        // Give A's in-flight page every chance to (wrongly) land, then assert
        // it never did.
        try await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertEqual(model.messages.count, 1, "A's late page must not have appended onto B's list")
        XCTAssertEqual(model.messages.first?.id, "\(accountIdB):only")
    }

    /// S64 review round 3: a sync reload (`reloadCurrentMessages`) landing while a
    /// load-more page is in flight rotates `listToken`, so that page's completion
    /// returns early on the token mismatch — before it can clear `isLoadingMore`.
    /// Unless the rotation itself releases that latch, every later
    /// `loadMoreMessagesIfNeeded` bails on `!isLoadingMore` and infinite scroll
    /// stays dead for the list until the user switches mailbox.
    func testASyncReloadDuringAnInFlightPageDoesNotWedgeInfiniteScroll() async throws {
        let model = makeModel()
        let (accountId, mailboxId) = try seedMessages(model: model, count: 1_200)
        model.select(mailbox: mailbox(accountId: accountId, mailboxId: mailboxId))
        try await waitUntil({ model.messages.count == 500 }, "first page never loaded")

        // Deterministic race: both calls run synchronously on the main actor up
        // to their `Task.detached` kick-off, and the page's completion needs the
        // main actor to land, so the reload ALWAYS rotates the token while the
        // page is still in flight — whichever of the two fetches finishes first.
        let generation = model.messagesGeneration
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        model.reloadCurrentMessages()
        try await waitUntil({ model.messagesGeneration > generation }, "the sync reload never landed")
        XCTAssertEqual(model.messages.count, 500, "the stale page must be dropped, not appended onto the reload")
        XCTAssertTrue(model.hasMoreMessages)

        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        try await waitUntil(
            { model.messages.count == 1_000 },
            "infinite scroll wedged: the reload that raced an in-flight page left isLoadingMore stuck")
        XCTAssertEqual(model.lastMessagesChange, .appended(500..<1_000))
    }

    /// A page requested while a sync reload is in flight was fetched from the
    /// PRE-reload cursor. If the reload brought in newer mail and lands first,
    /// appending that page would skip the rows the new mail pushed past the
    /// reload's limit — so the reload's replacement must invalidate it too. The
    /// two completions can land in either order; the list must end up a gap-free
    /// newest-first prefix either way. (Order-dependent: without the fix this is
    /// red only when the reload lands first, so it guards rather than proves.)
    func testAPageRequestedDuringAReloadNeverLeavesAGap() async throws {
        let model = makeModel()
        let (accountId, mailboxId) = try seedMessages(model: model, count: 1_200)
        model.select(mailbox: mailbox(accountId: accountId, mailboxId: mailboxId))
        try await waitUntil({ model.messages.count == 500 }, "first page never loaded")

        // New mail arrives (what a sync reload is FOR): newer than every seeded row.
        let newestId = "\(accountId):arrived-during-sync"
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: newestId,
            accountId: accountId, mailboxId: mailboxId,
            subject: "Arrived during sync", fromAddress: "sender@local.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false,
            sentAt: 1_800_000_000)

        model.reloadCurrentMessages()
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450) // still holds the pre-reload cursor
        try await waitUntil({ model.messages.first?.id == newestId }, "the sync reload never landed")
        try await Task.sleep(nanoseconds: 300_000_000) // let the page land (or be dropped)

        let newestFirst = [newestId] + (0..<1_200).reversed().map { "\(accountId):m-\($0)" }
        XCTAssertEqual(
            model.messages.map(\.id), Array(newestFirst.prefix(model.messages.count)),
            "the list must be a gap-free newest-first prefix")
    }

    // MARK: - Coordinator.apply

    func testApplyAfterAnAppendCallsInsertRowsAndPreservesSelection() async throws {
        let model = makeModel()
        let (accountId, mailboxId) = try seedMessages(model: model, count: 1_200)
        model.select(mailbox: mailbox(accountId: accountId, mailboxId: mailboxId))
        try await waitUntil({ model.messages.count == 500 }, "first page never loaded")
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        try await waitUntil({ model.messages.count == 1_000 }, "second page never appended")
        XCTAssertEqual(model.lastMessagesChange, .appended(500..<1_000))

        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        coord.rows = Array(model.messages.prefix(500)) // pre-append snapshot: range.lowerBound must match
        let table = SpyTableView()
        table.addTableColumn(NSTableColumn(identifier: NSUserInterfaceItemIdentifier("from")))
        table.dataSource = coord
        table.delegate = coord
        table.reloadData() // establish the pre-append 500 rows before appending
        table.selectRowIndexes(IndexSet(integer: 3), byExtendingSelection: false)
        let selectedId = coord.rows[3].id

        coord.apply(model: model, table: table)

        XCTAssertEqual(table.insertRowsCalls, 1)
        XCTAssertEqual(coord.rows.count, 1_000)
        XCTAssertEqual(table.selectedRowIndexes, IndexSet(integer: 3), "insertRows must not disturb the existing selection")
        XCTAssertEqual(coord.rows[3].id, selectedId)
    }

    func testApplyAfterAReplacementCallsReloadData() async throws {
        let model = makeModel()
        let (accountId, mailboxId) = try seedMessages(model: model, count: 5)
        model.select(mailbox: mailbox(accountId: accountId, mailboxId: mailboxId))
        try await waitUntil({ model.messages.count == 5 }, "the 5-message mailbox never loaded")
        XCTAssertEqual(model.lastMessagesChange, .replaced)

        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        // `rows` starts empty (a fresh coordinator, as `makeNSView` produces) —
        // deliberately NOT the append shape, so `apply` must take the reload path.
        let table = SpyTableView()
        table.addTableColumn(NSTableColumn(identifier: NSUserInterfaceItemIdentifier("from")))
        table.dataSource = coord
        table.delegate = coord

        coord.apply(model: model, table: table)

        XCTAssertEqual(table.reloadDataCalls, 1)
        XCTAssertEqual(table.insertRowsCalls, 0)
        XCTAssertEqual(coord.rows.count, 5)
    }
}
