// Castellum - Tests for FP15's empty-list explanations and the unsynced-folder
// helper.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class MessageListEmptyStateTests: XCTestCase {
    private func text(count: Int = 0, search: String = "", smart: Bool = false, selected: Bool = true,
                      syncing: Bool = false, unsynced: Bool = false) -> String? {
        MessageListEmptyState.text(
            messageCount: count, searchText: search, isSmartMailbox: smart, hasSelection: selected,
            isSyncing: syncing, mailboxNotSynced: unsynced)
    }

    func test_emptySearchShowsEmptyState() {
        XCTAssertEqual(text(search: "zzzz"), "No messages match “zzzz”")
    }

    func testEmptySmartMailboxAndPlainEmptyList() {
        XCTAssertEqual(text(smart: true), "No messages match this smart mailbox")
        XCTAssertEqual(text(), "No messages")
    }

    func testNothingToExplainWhenRowsExistOrNothingSelectedOrLoading() {
        XCTAssertNil(text(count: 3))
        XCTAssertNil(text(selected: false))
        XCTAssertNil(text(syncing: true))
    }

    func testUnsyncedFolderGetsTheSettingsHint() {
        XCTAssertEqual(text(unsynced: true),
                       "This folder isn’t synced. Choose it in Settings > Accounts > Folders…")
    }

    func testUnsyncedMailboxIsReportedNotSynced() {
        let mb = Mailbox(id: "a:Old", accountId: "a", path: "Old", name: "Old", role: nil,
                         delimiter: "/", subscribed: true, selectable: true, syncByDefault: false)
        XCTAssertFalse(SyncScope.isSynced(mailbox: mb, accountFilter: nil))
        XCTAssertTrue(SyncScope.isSynced(mailbox: mb, accountFilter: ["Old"]))
        XCTAssertFalse(SyncScope.isSynced(mailbox: mb, accountFilter: ["INBOX"]))
    }
}
