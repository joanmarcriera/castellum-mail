// Castellum - sortable message list (, slice 3): the model's sort
// state, persistence, keyset paging in the chosen order, and the table header
// following / driving the model. The Rust ordering itself is pinned in
// castellum-db `list_page.rs`; here the reference order is computed in Swift
// from the seeded rows.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class MessageSortValueTests: XCTestCase {
    private func suite() -> UserDefaults {
        let name = "castellum-sort-\(UUID().uuidString)"
        addTeardownBlock { UserDefaults.standard.removePersistentDomain(forName: name) }
        return UserDefaults(suiteName: name)!
    }

    func testDefaultIsNewestFirst() {
        XCTAssertEqual(MessageSort.default, MessageSort(key: .date, ascending: false))
        XCTAssertEqual(MessageSort.default.label, "date:desc")
    }

    func testSortSurvivesRelaunch() {
        let d = suite()
        MessageSort(key: .from, ascending: true).save(to: d)
        XCTAssertEqual(MessageSort.load(from: d), MessageSort(key: .from, ascending: true))
        MessageSort.default.save(to: d)
        XCTAssertNil(d.data(forKey: MessageSort.defaultsKey), "the default is stored as 'nothing'")
        XCTAssertEqual(MessageSort.load(from: d), .default)
    }

    func testCorruptStoredSortFallsBackToDateDescending() {
        let d = suite()
        d.set(Data("not json".utf8), forKey: MessageSort.defaultsKey)
        XCTAssertEqual(MessageSort.load(from: d), .default)
        d.set(Data(#"{"key":"sender-reputation","ascending":true}"#.utf8), forKey: MessageSort.defaultsKey)
        XCTAssertEqual(MessageSort.load(from: d), .default, "an unknown key (a future version) falls back")
    }

    func testToggleSameColumnFlipsDirection() {
        let s = MessageSort(key: .from, ascending: true).toggled(on: .from)
        XCTAssertEqual(s, MessageSort(key: .from, ascending: false))
        XCTAssertEqual(s.toggled(on: .from), MessageSort(key: .from, ascending: true))
        XCTAssertEqual(MessageSort.default.toggled(on: .date), MessageSort(key: .date, ascending: true))
    }

    func testFlagKeysMapToTheirColumnsAndStartGroupedFirst() {
        XCTAssertEqual(MessageSortKey.starred.columnId, "star")
        XCTAssertEqual(MessageSortKey.attachments.columnId, "attach")
        XCTAssertEqual(MessageSortKey(columnId: "star"), .starred)
        XCTAssertEqual(MessageSortKey(columnId: "attach"), .attachments)
        XCTAssertNil(MessageSortKey(columnId: "preview"))
        XCTAssertEqual(MessageSort.firstClick(on: .unread), MessageSort(key: .unread, ascending: true), "unread first")
        XCTAssertEqual(MessageSort.firstClick(on: .starred), MessageSort(key: .starred, ascending: false), "flagged first")
        XCTAssertEqual(MessageSort.firstClick(on: .attachments), MessageSort(key: .attachments, ascending: false))
        XCTAssertEqual(MessageSort(key: .starred, ascending: false).columnId, "star")
    }

    func testNewColumnStartsAtItsNaturalDirection() {
        XCTAssertEqual(MessageSort.default.toggled(on: .from), MessageSort(key: .from, ascending: true))
        XCTAssertEqual(MessageSort.default.toggled(on: .subject), MessageSort(key: .subject, ascending: true))
        XCTAssertEqual(MessageSort(key: .from, ascending: true).toggled(on: .date),
                       MessageSort(key: .date, ascending: false), "Received starts newest first")
    }
}

@MainActor
final class MessageListSortTests: XCTestCase {
    private var models: [AppModel] = []
    private var suites: [String] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-listsort-\(UUID().uuidString)", isDirectory: true)
        let model = AppModel(profileDirectory: dir)
        let name = "castellum-listsort-\(UUID().uuidString)"
        suites.append(name)
        model.messageSortDefaults = UserDefaults(suiteName: name)!
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for name in suites { UserDefaults.standard.removePersistentDomain(forName: name) }
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(_ cond: () -> Bool, timeout: TimeInterval = 8, _ what: String) async {
        let end = Date().addingTimeInterval(timeout)
        while !cond() && Date() < end { try? await Task.sleep(nanoseconds: 20_000_000) }
        if !cond() { XCTFail("timed out: \(what)") }
    }

    /// `count` rows with deliberately scrambled senders and subjects; `sentAt` increases with i.
    private func seed(model: AppModel, count: Int, role: String = "inbox")
        -> (accountId: String, mailbox: Mailbox, rows: [(id: String, from: String, subject: String, sentAt: Int64)])
    {
        let accountId = "sort-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try! DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId,
            email: "sort@local.test", mailboxId: mailboxId, role: role)
        var rows: [(String, String, String, Int64)] = []
        for i in 0..<count {
            let id = "\(accountId):m-\(i)"
            let from = "s\(String(format: "%02d", (i * 7) % 23))@local.test"
            let subject = "Topic \(String(format: "%04d", (i * 37) % 1000))"
            let sentAt = Int64(1_700_000_000 + i)
            try! DirectFixture.seedMessage(
                profileDirectory: model.profileDirectory, id: id, accountId: accountId, mailboxId: mailboxId,
                subject: subject, fromAddress: from,
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false,
                sentAt: sentAt)
            rows.append((id, from, subject, sentAt))
        }
        let mailbox = Mailbox(
            id: mailboxId, accountId: accountId, path: "INBOX", name: "INBOX", role: role,
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
        return (accountId, mailbox, rows)
    }

    /// The order the core must return for a text sort: key, then newest first, then id.
    private func reference(
        _ rows: [(id: String, from: String, subject: String, sentAt: Int64)], by sort: MessageSort
    ) -> [String] {
        rows.sorted { a, b in
            let (ka, kb): (String, String) = sort.key == .from ? (a.from, b.from) : (a.subject, b.subject)
            if sort.key == .date { return sort.ascending ? a.sentAt < b.sentAt : a.sentAt > b.sentAt }
            if ka != kb { return sort.ascending ? ka < kb : ka > kb }
            if a.sentAt != b.sentAt { return a.sentAt > b.sentAt }
            return a.id < b.id
        }.map(\.id)
    }

    func testSettingSortReloadsFromTheFirstPageInTheNewOrder() async throws {
        let model = makeModel()
        let (_, mailbox, rows) = seed(model: model, count: 40)
        model.select(mailbox: mailbox)
        await waitUntil({ model.messages.count == 40 }, "first load")
        XCTAssertEqual(model.messages.map(\.id), reference(rows, by: .default))

        model.setMessageSort(MessageSort(key: .from, ascending: true))
        await waitUntil({ model.messages.map(\.id) == self.reference(rows, by: MessageSort(key: .from, ascending: true)) },
                        "From A-Z order")
        model.setMessageSort(MessageSort(key: .subject, ascending: false))
        await waitUntil({ model.messages.map(\.id) == self.reference(rows, by: MessageSort(key: .subject, ascending: false)) },
                        "Subject Z-A order")
        model.toggleSort(column: "date")   // a new column starts newest first
        XCTAssertEqual(model.messageSort, MessageSort(key: .date, ascending: false))
        await waitUntil({ model.messages.map(\.id) == self.reference(rows, by: .default) }, "back to newest first")
    }

    func testLoadMoreContinuesInTheChosenOrder() async throws {
        let model = makeModel()
        let (_, mailbox, rows) = seed(model: model, count: 1_200)
        let sort = MessageSort(key: .from, ascending: true)
        model.setMessageSort(sort)
        model.select(mailbox: mailbox)
        await waitUntil({ model.messages.count == 500 }, "first page")
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        await waitUntil({ model.messages.count == 1_000 }, "second page")
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 950)
        await waitUntil({ model.messages.count == 1_200 }, "third page")
        let ids = model.messages.map(\.id)
        XCTAssertEqual(Set(ids).count, 1_200, "no duplicate rows across page boundaries")
        XCTAssertEqual(ids, reference(rows, by: sort), "three pages concatenate to the full From sort")
        XCTAssertFalse(model.hasMoreMessages)
    }

    func testSettingTheSortDropsThePageInFlightAndTheOldCursor() async throws {
        let model = makeModel()
        let (_, mailbox, _) = seed(model: model, count: 1_200)
        model.select(mailbox: mailbox)
        await waitUntil({ model.messages.count == 500 }, "first page")
        XCTAssertTrue(model.hasMoreMessages)
        let before = model.listToken
        model.setMessageSort(MessageSort(key: .subject, ascending: true))
        XCTAssertNotEqual(model.listToken, before, "a late page from the old sort is dropped by the token")
        XCTAssertFalse(model.hasMoreMessages, "the old cursor is never replayed under the new sort")
        await waitUntil({ model.hasMoreMessages && model.messages.count == 500 }, "reloaded first page")
    }

    func testSyncReloadKeepsTheSortAndTheLoadedDepth() async throws {
        let model = makeModel()
        let (_, mailbox, rows) = seed(model: model, count: 1_200)
        let sort = MessageSort(key: .subject, ascending: true)
        model.setMessageSort(sort)
        model.select(mailbox: mailbox)
        await waitUntil({ model.messages.count == 500 }, "first page")
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        await waitUntil({ model.messages.count == 1_000 }, "second page")
        model.reloadCurrentMessages()   // what a finished sync does
        let dispatched = model.listToken   // rotates once more when the reloaded rows land
        await waitUntil({ model.listToken != dispatched }, "reload completed")
        XCTAssertEqual(model.messages.map(\.id), Array(reference(rows, by: sort).prefix(1_000)),
                       "reload re-fetches the loaded depth in the same order")
        XCTAssertTrue(model.hasMoreMessages, "paging continues after the reload")
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 950)
        await waitUntil({ model.messages.count == 1_200 }, "third page after the reload")
        XCTAssertEqual(model.messages.map(\.id), reference(rows, by: sort))
    }

    func testSelectionIsKeptByIdAcrossASortChange() async throws {
        let model = makeModel()
        let (_, mailbox, rows) = seed(model: model, count: 30)
        model.select(mailbox: mailbox)
        await waitUntil({ model.messages.count == 30 }, "load")
        let pick = model.messages[5]
        model.selectedMessageIds = [pick.id]
        model.setMessageSort(MessageSort(key: .from, ascending: true))
        await waitUntil({ model.messages.map(\.id) == self.reference(rows, by: MessageSort(key: .from, ascending: true)) }, "resorted")
        XCTAssertEqual(model.selectedMessageIds, [pick.id], "the selection is by id, not by row")
    }

    func testFromIsNotSortableInSentAndDrafts() async throws {
        let model = makeModel()
        let (_, mailbox, rows) = seed(model: model, count: 10, role: "sent")
        model.setMessageSort(MessageSort(key: .from, ascending: true))
        model.select(mailbox: mailbox)
        XCTAssertEqual(model.sortableColumnIds, ["date", "subject", "unread", "star", "attach"])
        XCTAssertEqual(model.effectiveSort, .default, "the stored From sort does not apply inside Sent")
        await waitUntil({ model.messages.count == 10 }, "load")
        XCTAssertEqual(model.messages.map(\.id), reference(rows, by: .default))
        model.toggleSort(column: "from")   // not sortable here: ignored
        XCTAssertEqual(model.messageSort, MessageSort(key: .from, ascending: true))
        XCTAssertEqual(model.effectiveSort, .default)
    }

    func testUnreadFirstSort() async throws {
        let model = makeModel()
        let (accountId, mailbox, _) = seed(model: model, count: 12)
        // The fixture seeds everything unread; read every third message.
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: true)
        for i in stride(from: 0, to: 12, by: 3) {
            try model.core.markRead(messageId: "\(accountId):m-\(i)", isRead: true)
        }
        model.select(mailbox: mailbox)
        await waitUntil({ model.messages.count == 12 }, "load")
        model.toggleSort(column: "unread")
        XCTAssertEqual(model.messageSort, MessageSort(key: .unread, ascending: true))
        await waitUntil({ model.messages.prefix(8).allSatisfy { !$0.isRead } && model.messages.suffix(4).allSatisfy(\.isRead) },
                        "unread rows first")
        let unreadDates = model.messages.prefix(8).compactMap(\.sentAt)
        XCTAssertEqual(unreadDates, unreadDates.sorted(by: >), "newest first inside the group")
        model.toggleSort(column: "unread")   // flips: read rows first
        await waitUntil({ model.messages.prefix(4).allSatisfy(\.isRead) }, "read rows first")
    }

    /// A loaded row whose flag changes moves past the cursor in the Unread sort;
    /// the next page returns it again and it must not be listed twice.
    func testFlagChangeUnderUnreadSortDoesNotDuplicateOnTheNextPage() async throws {
        let model = makeModel()
        let (accountId, mailbox, _) = seed(model: model, count: 600)
        try model.core.setAccountWriteEnabled(accountId: accountId, enabled: true)
        model.setMessageSort(MessageSort(key: .unread, ascending: true))
        model.select(mailbox: mailbox)
        await waitUntil({ model.messages.count == 500 }, "first page")
        // Read a loaded row: it now sorts into the read group, after the cursor.
        try model.core.markRead(messageId: model.messages[0].id, isRead: true)
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        await waitUntil({ !model.hasMoreMessages }, "second page")
        let ids = model.messages.map(\.id)
        XCTAssertEqual(Set(ids).count, ids.count, "no row listed twice")
        XCTAssertEqual(ids.count, 600)
    }

    func testModelPersistsTheSort() async throws {
        let model = makeModel()
        model.setMessageSort(MessageSort(key: .subject, ascending: true))
        XCTAssertEqual(MessageSort.load(from: model.messageSortDefaults), MessageSort(key: .subject, ascending: true))
    }

    // MARK: - table header <-> model

    private func hostedList(model: AppModel) -> (MessageListView.Coordinator, NSTableView) {
        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        let table = NSTableView(frame: NSRect(x: 0, y: 0, width: 900, height: 400))
        for spec in kColumns {
            let c = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(spec.id))
            c.title = spec.title; c.width = spec.width
            if let key = MessageSortKey(rawValue: spec.id) {
                c.sortDescriptorPrototype = NSSortDescriptor(key: key.rawValue, ascending: MessageSort.firstClick(on: key).ascending)
            }
            table.addTableColumn(c)
        }
        table.dataSource = coord
        table.delegate = coord
        coord.table = table
        return (coord, table)
    }

    func testModelSortSetsTheTableSortDescriptors() {
        let model = makeModel()
        let (coord, table) = hostedList(model: model)
        coord.syncSortIndicator(table: table)
        XCTAssertEqual(table.sortDescriptors.first?.key, "date")
        XCTAssertEqual(table.sortDescriptors.first?.ascending, false)
        model.setMessageSort(MessageSort(key: .from, ascending: true))
        coord.syncSortIndicator(table: table)
        XCTAssertEqual(table.sortDescriptors.first?.key, "from")
        XCTAssertEqual(table.sortDescriptors.first?.ascending, true)
    }

    func testHeaderDescriptorChangeDrivesTheModel() {
        let model = makeModel()
        let (coord, table) = hostedList(model: model)
        coord.syncSortIndicator(table: table)   // also arms the coordinator
        table.sortDescriptors = [NSSortDescriptor(key: "subject", ascending: true)]   // what a header click does
        XCTAssertEqual(model.messageSort, MessageSort(key: .subject, ascending: true))
    }

    func testPushingTheModelSortDoesNotEchoBackIntoTheModel() {
        let model = makeModel()
        let (coord, table) = hostedList(model: model)
        // AppKit restored a stale descriptor from an earlier launch: the model owns the sort.
        table.sortDescriptors = [NSSortDescriptor(key: "from", ascending: false)]
        coord.syncSortIndicator(table: table)
        XCTAssertEqual(model.messageSort, .default)
        XCTAssertEqual(table.sortDescriptors.first?.key, "date")
    }

    func testFromHeaderLosesItsSortPrototypeInSent() async throws {
        let model = makeModel()
        let (_, mailbox, _) = seed(model: model, count: 3, role: "sent")
        model.select(mailbox: mailbox)
        let (coord, table) = hostedList(model: model)
        coord.syncSortIndicator(table: table)
        XCTAssertNil(table.tableColumns.first { $0.identifier.rawValue == "from" }?.sortDescriptorPrototype)
        XCTAssertNotNil(table.tableColumns.first { $0.identifier.rawValue == "date" }?.sortDescriptorPrototype)
        XCTAssertNil(table.tableColumns.first { $0.identifier.rawValue == "preview" }?.sortDescriptorPrototype)
    }
}
