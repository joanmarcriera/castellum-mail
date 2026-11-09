// Castellum - S34: the Account column must show automatically in a unified
// (cross-account) list, and restore exactly the state it found once the list
// stops being unified.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

@MainActor
final class UnifiedListColumnTests: XCTestCase {
    private func accountColumn(hidden: Bool) -> NSTableColumn {
        let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(AccountColumn.columnId))
        col.isHidden = hidden
        return col
    }

    private func table(withAccountColumnHidden hidden: Bool) -> NSTableView {
        let t = NSTableView()
        t.addTableColumn(accountColumn(hidden: hidden))
        return t
    }

    private func coordinator() -> MessageListView.Coordinator {
        MessageListView.Coordinator(model: AppModel(profileDirectory:
            TestScratch.root.appendingPathComponent("castellum-col-\(UUID().uuidString)")),
            onOpen: {})
    }

    func testForcesTheHiddenAccountColumnVisibleForAUnifiedList() {
        let coord = coordinator()
        let t = table(withAccountColumnHidden: true)
        coord.syncAccountColumnVisibility(table: t, unified: true)
        XCTAssertFalse(t.tableColumns[0].isHidden)
        XCTAssertTrue(coord.accountColumnForcedVisible)
    }

    func testRestoresHiddenOnlyWhenThisCoordinatorForcedIt() {
        let coord = coordinator()
        let t = table(withAccountColumnHidden: true)
        coord.syncAccountColumnVisibility(table: t, unified: true) // force visible
        coord.syncAccountColumnVisibility(table: t, unified: false) // leave unified
        XCTAssertTrue(t.tableColumns[0].isHidden, "must restore exactly what it found")
        XCTAssertFalse(coord.accountColumnForcedVisible)
    }

    func testNeverTouchesAnAlreadyVisibleColumnsFlag() {
        let coord = coordinator()
        let t = table(withAccountColumnHidden: false)
        coord.syncAccountColumnVisibility(table: t, unified: true)
        XCTAssertFalse(t.tableColumns[0].isHidden)
        XCTAssertFalse(coord.accountColumnForcedVisible, "the user's own visible choice was never forced")
        coord.syncAccountColumnVisibility(table: t, unified: false)
        XCTAssertFalse(t.tableColumns[0].isHidden, "leaving unified must not hide a column the user chose to show")
    }
}
