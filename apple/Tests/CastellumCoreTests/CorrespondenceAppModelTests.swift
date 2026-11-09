// Castellum - AppModel state rules for the Correspondence view (MM-4 review).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class CorrespondenceAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-corr-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    private let inbox = Mailbox(
        id: "a1:INBOX", accountId: "a1", path: "INBOX", name: "INBOX", role: "inbox",
        delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    /// Mailbox-wide actions have no target while Correspondence hides the mailbox.
    func testActionMailboxIsNilInCorrespondence() {
        let model = makeModel()
        model.selectedMailbox = inbox
        XCTAssertEqual(model.actionMailbox, inbox)
        XCTAssertEqual(model.emptyTrashAccountId, "a1")
        model.correspondence = CorrespondenceContext(address: "a@x.test", name: nil)
        // (setting correspondence directly leaves selectedMailbox set, as
        // showCorrespondence does)
        XCTAssertEqual(model.selectedMailbox, inbox)
        XCTAssertNil(model.actionMailbox)
        XCTAssertNil(model.emptyTrashAccountId)
    }

    /// Mark All Read must not touch the optimistic state of the visible rows
    /// (nor call the core) in Correspondence.
    func testMarkAllReadIsANoOpInCorrespondence() {
        let model = makeModel()
        model.selectedMailbox = inbox
        model.correspondence = CorrespondenceContext(address: "a@x.test", name: nil)
        model.messages = [MessageHeader(
            id: "m1", accountId: "a1", mailboxId: "a1:INBOX", subject: "s", fromName: nil,
            fromAddress: "a@x.test", sentAt: 1, snippet: nil, hasAttachments: false, hasBody: nil,
            isRead: false, isStarred: false, threadCount: 1)]
        model.markAllReadInCurrentMailbox()
        XCTAssertFalse(model.messages[0].isRead)
    }

    /// The "came from Snoozed" memory ends whenever Correspondence does, however
    /// it ends (here: sidebar navigation), so a later close cannot jump to Snoozed.
    func testCameFromSnoozedResetsWhenCorrespondenceEndsByNavigation() {
        let model = makeModel()
        model.selectedIsSnoozed = true
        model.showCorrespondence(address: "a@x.test", name: nil)
        XCTAssertFalse(model.selectedIsSnoozed)
        model.selectedMailbox = inbox // sidebar navigation clears correspondence
        XCTAssertNil(model.correspondence)
        model.showCorrespondence(address: "b@x.test", name: nil) // not from Snoozed this time
        model.closeCorrespondence()
        XCTAssertFalse(model.selectedIsSnoozed, "stale flag must not send the user to Snoozed")
    }

    func testCloseReturnsToSnoozedWhenItCameFromThere() {
        let model = makeModel()
        model.selectedIsSnoozed = true
        model.showCorrespondence(address: "a@x.test", name: nil)
        model.closeCorrespondence()
        XCTAssertTrue(model.selectedIsSnoozed)
    }

    func testMyAddressesIncludeIdentities() {
        let model = makeModel()
        model.identities = ["a1": [Identity(id: 1, accountId: "a1", email: "Alias@X.test", displayName: nil)]]
        XCTAssertTrue(model.myAddresses.contains("alias@x.test"))
    }
}
