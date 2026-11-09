// Castellum - what NSTableView column autosave actually persists and restores.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The message list has now twice been asked "does adding a column require
// bumping `autosaveName`?", and bumping is not free: it orphans the saved
// layout and drops every user back to factory widths. This pins the real
// behaviour so the next column addition is a decision, not a guess.
//
// The stored payload shape (an array of {Identifier, Width, Hidden}) was
// confirmed by decoding a real one out of `uk.co.riera.castellum`.

import AppKit
import XCTest
@testable import Castellum

final class ColumnAutosaveTests: XCTestCase {
    private var autosaveName = ""

    override func setUp() {
        super.setUp()
        autosaveName = "CastellumAutosaveTest-\(UUID().uuidString)"
    }

    override func tearDown() {
        for name in suites { UserDefaults.standard.removePersistentDomain(forName: name) }
        UserDefaults.standard.removeObject(forKey: "NSTableView Columns v3 \(autosaveName)")
        super.tearDown()
    }

    private func table(_ ids: [String]) -> NSTableView {
        let t = NSTableView()
        // Match the message list: without this, NSTableView's default
        // `.uniformColumnAutoresizingStyle` redistributes widths after the
        // restore and the test measures autoresizing, not autosave.
        t.columnAutoresizingStyle = .noColumnAutoresizing
        for id in ids {
            let c = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(id))
            c.width = 100
            t.addTableColumn(c)
        }
        return t
    }

    /// THE question: does a column the saved layout has never heard of still
    /// appear? If it does, bumping `autosaveName` to introduce a column is
    /// pure cost and the one-shot migration in `MessageListView` is the right
    /// mechanism.
    func testAColumnAbsentFromSavedDataStaysVisible() {
        // Session 1: two columns, one resized, then saved.
        let first = table(["from", "subject"])
        first.autosaveTableColumns = true
        first.autosaveName = autosaveName
        first.tableColumns[0].width = 137
        first.tableColumns[1].isHidden = true

        // Session 2: the same table gains a third column it has never saved.
        let second = table(["from", "subject", "account"])
        second.autosaveTableColumns = true
        second.autosaveName = autosaveName

        let account = second.tableColumns.first { $0.identifier.rawValue == "account" }
        XCTAssertNotNil(account, "the new column must still exist after a restore")
        XCTAssertFalse(
            account?.isHidden ?? true,
            "a column absent from the saved layout must not be hidden by the restore — "
                + "if this fails, adding a column DOES require an autosaveName bump"
        )
    }

    /// The other half: the restore must actually carry the saved width and
    /// hidden state across, or "bumping loses the user's layout" would be a
    /// non-problem and the migration would be pointless.
    func testSavedWidthAndHiddenStateAreRestored() {
        let first = table(["from", "subject"])
        first.autosaveTableColumns = true
        first.autosaveName = autosaveName
        first.tableColumns[0].width = 137
        first.tableColumns[1].isHidden = true

        let second = table(["from", "subject"])
        second.autosaveTableColumns = true
        second.autosaveName = autosaveName

        XCTAssertEqual(second.tableColumns[0].width, 137, accuracy: 0.5,
                       "width must survive; this is what a name bump throws away")
        XCTAssertTrue(second.tableColumns[1].isHidden,
                      "hidden state is persisted — which is why the header menu "
                          + "must sync its ticks from the restored columns")
    }

    // MARK: - #3042 default layout fits the pane, Received stays visible

    private var suites: [String] = []

    @MainActor
    private func isolatedDefaults() -> UserDefaults {
        let name = "castellum-colfit-\(UUID().uuidString)"
        suites.append(name)
        return UserDefaults(suiteName: name)!
    }

    /// A message-list table hosted like `makeNSView` hosts it: the real `kColumns`, `.inset` style,
    /// inside a scroll view `width` points wide (the list pane), with a coordinator attached.
    @MainActor
    private func hostedList(width: CGFloat, longRows: Bool = false)
        -> (MessageListView.Coordinator, NSTableView, NSScrollView)
    {
        let model = AppModel(profileDirectory: TestScratch.root
            .appendingPathComponent("castellum-colfit-\(UUID().uuidString)"))
        if longRows {
            model.messages = (0..<20).map { i in
                MessageHeader(
                    id: "m\(i)", accountId: "a", mailboxId: "a:INBOX",
                    subject: String(repeating: "A very long subject line that keeps going ", count: 6),
                    fromName: "Somebody With A Rather Long Display Name",
                    fromAddress: "x@example.com", sentAt: 1_700_000_000,
                    snippet: String(repeating: "Preview text that is also very long. ", count: 12),
                    hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
            }
        }
        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: width, height: 600))
        scroll.hasVerticalScroller = false
        let table = NSTableView(frame: scroll.bounds)
        table.style = .inset
        MessageListView.applyColumnSpacing(table)
        table.columnAutoresizingStyle = .noColumnAutoresizing
        for spec in kColumns {
            let c = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(spec.id))
            c.title = spec.title; c.width = spec.width; c.minWidth = spec.min; c.maxWidth = spec.max
            table.addTableColumn(c)
        }
        scroll.documentView = table
        table.dataSource = coord
        table.delegate = coord
        coord.table = table
        if longRows { coord.apply(model: model, table: table) }
        return (coord, table, scroll)
    }

    @MainActor
    private func dateRightEdge(_ table: NSTableView) -> CGFloat? {
        guard let i = table.tableColumns.firstIndex(where: { $0.identifier.rawValue == "date" }),
              !table.tableColumns[i].isHidden else { return nil }
        return table.rect(ofColumn: i).maxX
    }

    @MainActor
    private func assertDateInside(_ table: NSTableView, _ width: CGFloat, _ note: String = "") {
        guard let edge = dateRightEdge(table) else { return XCTFail("Received column hidden \(note)") }
        XCTAssertLessThanOrEqual(edge, width + 0.5, "Received's right edge is outside the \(width) pt pane \(note)")
    }

    /// Widescreen pane (709 pt): a brand-new install must show Received.
    @MainActor
    func testDefaultVisibleColumnsFitWidescreenListPane() {
        let (coord, table, _) = hostedList(width: 709)
        coord.layoutDefaults = isolatedDefaults()
        coord.savedLayoutExisted = false
        assertDateInside(table, 709 + 1_000, "(sanity: wide)")
        XCTAssertGreaterThan(dateRightEdge(table) ?? 0, 709, "precondition: factory widths overflow 709 pt")
        coord.applyFirstLayoutIfNeeded(available: 709)
        assertDateInside(table, 709)
    }

    /// Default 1200 pt window: the Widescreen list pane is 500 pt. Received must be inside,
    /// From/Subject visible, and the hidden Tags column must not come back next launch.
    @MainActor
    func testDefaultWindowPaneFiveHundredShowsReceived() {
        let (coord, table, _) = hostedList(width: 500)
        let defaults = isolatedDefaults()
        coord.layoutDefaults = defaults
        coord.savedLayoutExisted = false
        coord.applyFirstLayoutIfNeeded(available: 500)
        assertDateInside(table, 500)
        for id in ["from", "subject"] {
            XCTAssertFalse(table.tableColumns.first { $0.identifier.rawValue == id }!.isHidden, "\(id) visible")
        }
        XCTAssertTrue(table.tableColumns.first { $0.identifier.rawValue == "tags" }!.isHidden)
        XCTAssertTrue(defaults.bool(forKey: "CastellumMessageList.tagsColumnAdded"),
                      "tags migration is marked done so the hidden Tags column is not re-shown at next launch")
    }

    /// Classic pane (1069 pt): still visible, and nothing needs hiding there.
    @MainActor
    func testDefaultVisibleColumnsFitClassicListPane() {
        let (coord, table, _) = hostedList(width: 1069)
        coord.layoutDefaults = isolatedDefaults()
        coord.savedLayoutExisted = false
        coord.listCanBeSqueezed = { false }   // Classic: the list keeps the window's width
        coord.applyFirstLayoutIfNeeded(available: 1069)
        assertDateInside(table, 1069)
        XCTAssertTrue(table.tableColumns.allSatisfy { !$0.isHidden }, "1069 pt holds every column; nothing hides")
    }

    /// The unified list force-shows Account AFTER the first fit; Received must still fit at 709 pt.
    @MainActor
    func testUnifiedListKeepsDateInsideWidescreenPane() {
        let (coord, table, _) = hostedList(width: 709)
        coord.layoutDefaults = isolatedDefaults()
        coord.savedLayoutExisted = false
        coord.applyFirstLayoutIfNeeded(available: 709)
        coord.syncAccountColumnVisibility(table: table, unified: true)
        XCTAssertFalse(table.tableColumns.first { $0.identifier.rawValue == "account" }!.isHidden)
        assertDateInside(table, 709, "(unified, Account forced visible)")
    }

    /// Fit Columns on long content must not push Received out of the pane.
    @MainActor
    func testFitColumnsKeepsDateInsidePane() {
        let (coord, table, _) = hostedList(width: 709, longRows: true)
        // The widescreen default hides Preview and Account (they cannot fit at 709 pt);
        // Fit Columns shrinks the rest and hides nothing further.
        for id in ["preview", "account"] { table.tableColumns.first { $0.identifier.rawValue == id }!.isHidden = true }
        let hiddenBefore = table.tableColumns.filter(\.isHidden).count
        coord.fitAllColumns()
        assertDateInside(table, 709)
        XCTAssertEqual(table.tableColumns.filter(\.isHidden).count, hiddenBefore, "Fit Columns hides nothing")
    }

    /// M1: a saved layout whose Received column is on-screen is not touched.
    @MainActor
    func testSavedLayoutWithVisibleDateIsLeftAlone() {
        let (coord, table, _) = hostedList(width: 1600)
        let defaults = isolatedDefaults()
        coord.layoutDefaults = defaults
        coord.savedLayoutExisted = true
        let before = table.tableColumns.map { [$0.width, $0.isHidden ? 1 : 0] }
        coord.applyFirstLayoutIfNeeded(available: 1600)
        XCTAssertEqual(table.tableColumns.map { [$0.width, $0.isHidden ? 1 : 0] }, before)
        XCTAssertTrue(defaults.bool(forKey: MessageListView.dateFitAppliedKey), "the guard key is set either way")
    }

    /// M1: a saved layout with Received off-screen is shrunk ONCE, hiding nothing.
    @MainActor
    func testSavedLayoutWithOffscreenDateIsShrunkOnce() {
        let (coord, table, _) = hostedList(width: 709)
        let defaults = isolatedDefaults()
        coord.layoutDefaults = defaults
        coord.savedLayoutExisted = true
        // A saved layout that hid Account and widened Preview on purpose.
        table.tableColumns.first { $0.identifier.rawValue == "account" }!.isHidden = true
        table.tableColumns.first { $0.identifier.rawValue == "preview" }!.width = 900
        XCTAssertGreaterThan(dateRightEdge(table) ?? 0, 709)
        coord.applyFirstLayoutIfNeeded(available: 709)
        assertDateInside(table, 709)
        XCTAssertEqual(table.tableColumns.filter(\.isHidden).map { $0.identifier.rawValue }, ["account"],
                       "M1 hides nothing it did not find hidden")
        // Second run (the key is set): a user who widens Preview again keeps it.
        table.tableColumns.first { $0.identifier.rawValue == "preview" }!.width = 900
        coord.applyFirstLayoutIfNeeded(available: 709)
        XCTAssertEqual(table.tableColumns.first { $0.identifier.rawValue == "preview" }!.width, 900)
    }

    // MARK: - #3042 follow-up: the pane narrows after the first layout

    /// Widescreen at the default window: the first layout sees 500 pt, then selecting a
    /// message squeezes the list to 420 pt. Received must be inside both, in a single
    /// mailbox and in All Inboxes (Account forced on).
    @MainActor
    func testReceivedStaysInsideWhenSelectionSqueezesThePane() {
        for unified in [false, true] {
            let (coord, table, scroll) = hostedList(width: 500)
            coord.layoutDefaults = isolatedDefaults()
            coord.savedLayoutExisted = false
            coord.listCanBeSqueezed = { true }
            coord.applyFirstLayoutIfNeeded(available: 500)
            if unified { coord.syncAccountColumnVisibility(table: table, unified: true) }
            assertDateInside(table, 500, "(first layout, unified=\(unified))")
            scroll.setFrameSize(NSSize(width: 420, height: 600))
            coord.keepReceivedVisible(available: 420)
            assertDateInside(table, 420, "(squeezed, unified=\(unified))")
        }
    }

    /// A roomy first layout (790 pt, as at an 1800 pt window) is squeezed to 420 pt too.
    @MainActor
    func testRoomyFirstLayoutSurvivesTheSqueeze() {
        let (coord, table, _) = hostedList(width: 790)
        coord.layoutDefaults = isolatedDefaults()
        coord.savedLayoutExisted = false
        coord.listCanBeSqueezed = { true }
        coord.applyFirstLayoutIfNeeded(available: 790)
        coord.syncAccountColumnVisibility(table: table, unified: true)
        assertDateInside(table, 790)
        coord.keepReceivedVisible(available: 420)
        assertDateInside(table, 420, "(unified, squeezed)")
    }

    /// The pane-resize pass only shrinks: it never hides a column, even when it cannot
    /// bring Received back (the user showed every column on a narrow pane).
    @MainActor
    func testPaneResizePassNeverHides() {
        let (coord, table, _) = hostedList(width: 420)
        let before = table.tableColumns.map(\.isHidden)
        coord.keepReceivedVisible(available: 420)
        XCTAssertEqual(table.tableColumns.map(\.isHidden), before)
        let from = table.tableColumns.first { $0.identifier.rawValue == "from" }!
        XCTAssertEqual(from.width, from.minWidth, accuracy: 0.01, "it did shrink")
    }

    /// The reported failure: All Inboxes at 500 (and 420) pt, every column the old
    /// first-run left visible plus Account forced, then View > Fit Columns.
    @MainActor
    func testFitColumnsBringsReceivedBackInAllInboxes() {
        for width: CGFloat in [500, 420] {
            let (coord, table, _) = hostedList(width: width, longRows: true)
            for id in ["preview", "thread", "tags"] {
                table.tableColumns.first { $0.identifier.rawValue == id }!.isHidden = true
            }
            coord.syncAccountColumnVisibility(table: table, unified: true)
            coord.fitAllColumns()
            assertDateInside(table, width, "(Fit Columns, All Inboxes)")
            XCTAssertFalse(table.tableColumns.first { $0.identifier.rawValue == "account" }!.isHidden,
                           "Fit Columns never hides the forced Account column")
        }
    }

    /// Fit Columns with every column shown on a 420 pt pane hides as a last resort.
    @MainActor
    func testFitColumnsHidesAsALastResort() {
        let (coord, table, _) = hostedList(width: 420, longRows: true)
        coord.fitAllColumns()
        assertDateInside(table, 420)
        XCTAssertTrue(table.tableColumns.first { $0.identifier.rawValue == "preview" }!.isHidden)
        if let menu = table.headerView?.menu {   // header ticks follow the hidden state
            for item in menu.items { if let c = item.representedObject as? NSTableColumn {
                XCTAssertEqual(item.state == .off, c.isHidden) } }
        }
    }

    /// #2245 wiring: the real cell provider puts the dot in the Subject cell exactly
    /// when the Tags column is hidden (the new-install layout hides it), and the
    /// header-menu toggle redraws it.
    @MainActor
    func testTaggedRowShowsItsDotInTheSubjectWhileTagsIsHidden() {
        let (coord, table, _) = hostedList(width: 500, longRows: true)
        coord.model.messages[0].keywords = ["Work"]
        coord.apply(model: coord.model, table: table)
        let tagsCol = table.tableColumns.first { $0.identifier.rawValue == "tags" }!
        let subjectCol = table.tableColumns.first { $0.identifier.rawValue == "subject" }!
        func subjectText() -> String {
            (coord.tableView(table, viewFor: subjectCol, row: 0) as? NSTextField)?.stringValue ?? "<none>"
        }
        tagsCol.isHidden = true
        XCTAssertTrue(subjectText().hasPrefix("\u{25CF} "), "dot leads the subject: \(subjectText())")
        tagsCol.isHidden = false
        XCTAssertFalse(subjectText().contains("\u{25CF}"), "no second dot while the Tags column shows it")
    }
}
