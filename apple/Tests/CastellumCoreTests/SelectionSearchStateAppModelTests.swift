// Castellum - Tests for FP14: selecting a list resets the search text, and the
// smart-mailbox / Snoozed selections are mutually exclusive.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class SelectionSearchStateAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-selsearch-\(UUID().uuidString)")
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

    private func mailbox() -> Mailbox {
        Mailbox(id: "a:INBOX", accountId: "a", path: "INBOX", name: "INBOX", role: "inbox",
                delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    private func smart() -> SmartMailbox {
        SmartMailbox(id: "sm1", name: "Foxes", query: "fox", group: nil, scope: nil,
                     sortOrder: 0, symbol: nil, cachedTotal: 0, cachedUnread: 0,
                     countsCapped: false, countsUpdatedAt: 0, createdAt: 0, updatedAt: 0)
    }

    func testEverySelectVariantClearsTheSearchText() {
        let model = makeModel()
        model.searchText = "fox"
        model.select(mailbox: mailbox())
        XCTAssertEqual(model.searchText, "")

        model.searchText = "fox"
        model.select(unifiedRole: "inbox")
        XCTAssertEqual(model.searchText, "")

        model.searchText = "fox"
        model.select(snoozed: ())
        XCTAssertEqual(model.searchText, "")

        model.searchText = "fox"
        model.select(smartMailbox: smart())
        XCTAssertEqual(model.searchText, "")
    }

    func testVipsViewStillKeepsItsQuery() {
        let model = makeModel()
        model.selectVips()
        XCTAssertEqual(model.searchText, "is:vip")
        XCTAssertTrue(model.isVipViewSelected)
    }

    func testSmartThenSnoozedClearsTheSmartSelection() {
        let model = makeModel()
        model.select(smartMailbox: smart())
        model.select(snoozed: ())
        XCTAssertTrue(model.selectedIsSnoozed)
        XCTAssertNil(model.selectedSmartMailboxId)
    }

    func testSnoozedThenSmartClearsTheSnoozedSelection() {
        let model = makeModel()
        model.select(snoozed: ())
        model.select(smartMailbox: smart())
        XCTAssertFalse(model.selectedIsSnoozed)
        XCTAssertEqual(model.selectedSmartMailboxId, "sm1")
    }
}
