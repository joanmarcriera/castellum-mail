// Castellum - a reload that drops rows must shrink a multi-selection to the
// survivors (defect list-reload-leaves-stale-multi-selection-ids, packet FP25).
// Without it Delete/Archive/flag act on messages the user can no longer see,
// because `isReloading` suppresses the table's selection callback.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

@MainActor
final class MessageListSelectionReloadTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-selreload-\(UUID().uuidString)", isDirectory: true)
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

    private func header(_ id: String) -> MessageHeader {
        MessageHeader(id: id, accountId: "a", mailboxId: "a:INBOX",
                      subject: id, fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
    }

    /// Off-screen table wired to a coordinator showing `[A, B, C]` with all three selected, C primary.
    private func setUp3(_ model: AppModel) -> (MessageListView.Coordinator, NSTableView) {
        model.messages = [header("A"), header("B"), header("C")]
        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        let table = NSTableView()
        table.addTableColumn(NSTableColumn(identifier: NSUserInterfaceItemIdentifier("from")))
        table.allowsMultipleSelection = true
        table.dataSource = coord
        table.delegate = coord
        coord.table = table   // makeNSView normally sets this; without it restoreOrAdvance is a no-op
        coord.apply(model: model, table: table)
        table.selectRowIndexes(IndexSet(0...2), byExtendingSelection: false)
        model.selectedMessageIds = ["A", "B", "C"]
        model.selectedMessage = model.messages[2]
        return (coord, table)
    }

    private func drainMainQueue() async {
        for _ in 0..<3 {
            await withCheckedContinuation { c in DispatchQueue.main.async { c.resume() } }
        }
    }

    func testReloadThatDropsPartOfAMultiSelectionShrinksModelSelection() async {
        let model = makeModel()
        let (coord, table) = setUp3(model)
        XCTAssertEqual(model.selectedMessageIds, ["A", "B", "C"])

        model.messages = [header("A")]
        coord.apply(model: model, table: table)
        await drainMainQueue()

        XCTAssertEqual(model.selectedMessageIds, ["A"])
        XCTAssertEqual(model.currentSelectionIds(), ["A"], "Delete/Archive must only act on the visible survivor")
    }

    func testReloadThatDropsThePrimaryOfAMultiSelectionMovesThePrimary() async {
        let model = makeModel()
        let (coord, table) = setUp3(model)

        model.messages = [header("A"), header("B")]
        coord.apply(model: model, table: table)
        await drainMainQueue()

        XCTAssertEqual(model.selectedMessageIds, ["A", "B"])
        XCTAssertNotEqual(model.selectedMessage?.id, "C", "the open message must be one still in the list")
    }

    // MARK: #3448

    /// Switching mailbox clears the model's selection; a row that survives in the new list
    /// must not stay highlighted with an empty reading pane.
    func testClearedModelSelectionDropsTheTableHighlightOnReload() {
        let model = makeModel()
        let (coord, table) = setUp3(model)
        model.clearSelection()

        model.messages = [header("B"), header("C")]  // the old rows survive in the new list
        coord.apply(model: model, table: table)

        XCTAssertTrue(table.selectedRowIndexes.isEmpty, "no highlighted row without a model selection")
        XCTAssertNil(model.selectedMessage)
    }

    /// Archive auto-advance (selection intact in the model) must keep working.
    func testAutoAdvanceStillWorksWhenTheModelSelectionIsIntact() async {
        let model = makeModel()
        let (coord, table) = setUp3(model)
        table.selectRowIndexes(IndexSet(integer: 1), byExtendingSelection: false)
        model.selectedMessageIds = ["B"]
        model.selectedMessage = model.messages[1]

        model.messages = [header("A"), header("C")]  // B archived
        coord.apply(model: model, table: table)
        await drainMainQueue()

        XCTAssertEqual(table.selectedRowIndexes.count, 1, "selection advanced to a neighbour")
    }

    /// The header menu's ticks follow the real column state; the forced Account column
    /// cannot be toggled while it is forced.
    func testHeaderMenuTicksFollowColumnStateAndForcedAccountIsLocked() {
        let model = makeModel()
        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        let table = NSTableView()
        let account = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(AccountColumn.columnId))
        let from = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("from"))
        table.addTableColumn(account)
        table.addTableColumn(from)
        coord.table = table

        let menu = NSMenu()
        for col in [account, from] {
            let item = NSMenuItem(title: col.identifier.rawValue, action: nil, keyEquivalent: "")
            item.representedObject = col
            item.state = .off
            menu.addItem(item)
        }
        account.isHidden = false          // forced visible in All Inboxes
        coord.accountColumnForcedVisible = true
        from.isHidden = true
        coord.syncHeaderMenuTicks(menu)

        XCTAssertEqual(menu.items[0].state, .on, "visible column shows its tick")
        XCTAssertFalse(menu.items[0].isEnabled, "forced column is locked")
        XCTAssertEqual(menu.items[1].state, .off)
        XCTAssertTrue(menu.items[1].isEnabled)

        coord.toggleColumn(menu.items[0])
        XCTAssertFalse(account.isHidden, "a click cannot hide the forced column")
    }

    func testReloadingTheTagsColumnKeepsTheSelection() {
        let model = makeModel()
        let (coord, table) = setUp3(model)
        table.addTableColumn(NSTableColumn(identifier: NSUserInterfaceItemIdentifier("tags")))
        coord.reloadTagsColumn(table: table)
        XCTAssertEqual(table.selectedRowIndexes, IndexSet(0...2))
    }
}
