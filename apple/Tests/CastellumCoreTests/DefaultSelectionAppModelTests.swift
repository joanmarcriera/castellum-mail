// Castellum - Tests for the default mailbox selection (FP02): a window with
// nothing selected shows the unified inbox, but never steals a selection.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class DefaultSelectionAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-defaultsel-\(UUID().uuidString)")
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

    private func account(_ id: String) -> Account {
        Account(id: id, email: "\(id)@local.test", displayName: nil, provider: "imap")
    }

    func testNothingSelectedWithAnAccountSelectsTheUnifiedInbox() {
        let model = makeModel()
        model.accounts = [account("a")]
        model.selectDefaultMailboxIfNothingSelected()
        XCTAssertEqual(model.selectedUnifiedRole, "inbox")
    }

    func testNoAccountsSelectsNothing() {
        let model = makeModel()
        model.selectDefaultMailboxIfNothingSelected()
        XCTAssertNil(model.selectedUnifiedRole)
    }

    func testAnExistingSelectionIsNeverStolen() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.selectedIsSnoozed = true
        model.selectDefaultMailboxIfNothingSelected()
        XCTAssertNil(model.selectedUnifiedRole, "Snoozed stays selected when a second account lands")

        let other = makeModel()
        other.accounts = [account("a")]
        other.searchText = "fox"
        other.selectDefaultMailboxIfNothingSelected()
        XCTAssertNil(other.selectedUnifiedRole, "an active search is left alone")
    }
}
