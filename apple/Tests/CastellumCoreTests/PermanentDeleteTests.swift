// Castellum - Delete inside Trash deletes permanently, only after confirmation
// (S73 /). The Rust side proves the queue op and the expunge;
// these prove the UI gating: no confirmation -> nothing queued, non-Trash ->
// still a move to Trash, failure -> the message stays visible.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class PermanentDeleteTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        // Temp profile + in-memory Keychain (TestHost.isActive): never the real ones.
        let dir = TestScratch.root
            .appendingPathComponent("castellum-permdel-\(UUID().uuidString)")
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

    private func mailbox(_ id: String, role: String) -> Mailbox {
        Mailbox(id: id, accountId: "a", path: id, name: id, role: role,
                delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    private func header(_ id: String, mailboxId: String) -> MessageHeader {
        MessageHeader(id: id, accountId: "a", mailboxId: mailboxId,
                      subject: "s", fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
    }

    func testOnlyTheTrashRoleAsksForPermanentDeletion() {
        XCTAssertEqual(DeleteDisposition.forMailboxRole("trash"), .confirmPermanent)
        for role in ["inbox", "archive", "sent", "drafts", "junk", nil] {
            XCTAssertEqual(DeleteDisposition.forMailboxRole(role), .moveToTrash, "\(role ?? "nil")")
        }
    }

    func testRowRolesDecideTheDisposition() {
        XCTAssertEqual(DeleteDisposition.forRowMailboxRoles(["inbox"]), .moveToTrash)
        XCTAssertEqual(DeleteDisposition.forRowMailboxRoles(["trash", "inbox"]), .moveToTrash)
        XCTAssertEqual(DeleteDisposition.forRowMailboxRoles(["trash", nil]), .moveToTrash)
        XCTAssertEqual(DeleteDisposition.forRowMailboxRoles([]), .moveToTrash)
        XCTAssertEqual(DeleteDisposition.forRowMailboxRoles(["trash", "trash"]), .confirmPermanent)
    }

    func testInboxRowSelectedWhileSidebarShowsTrashStillMovesToTrash() {
        let model = makeModel()
        let m = header("a:INBOX:1", mailboxId: "a:INBOX")
        model.mailboxes = ["a": [mailbox("a:INBOX", role: "inbox"), mailbox("a:Trash", role: "trash")]]
        model.selectedMailbox = mailbox("a:Trash", role: "trash")   // stale sidebar mailbox
        model.messages = [m]
        model.selectedMessageIds = [m.id]

        model.deleteSelected()

        XCTAssertNil(model.pendingPermanentDelete, "a live message is never offered permanent erase")
    }

    func testTrashRowSelectedWhileSidebarShowsInboxAsksForConfirmation() {
        let model = makeModel()
        let m = header("a:Trash:1", mailboxId: "a:Trash")
        model.mailboxes = ["a": [mailbox("a:INBOX", role: "inbox"), mailbox("a:Trash", role: "trash")]]
        model.selectedMailbox = mailbox("a:INBOX", role: "inbox")
        model.messages = [m]
        model.selectedMessageIds = [m.id]

        model.deleteSelected()

        XCTAssertEqual(model.pendingPermanentDelete?.ids, [m.id])
    }

    func testDeleteInTrashOnlyAsksForConfirmationAndChangesNothing() {
        let model = makeModel()
        let m = header("a:Trash:1", mailboxId: "a:Trash")
        model.selectedMailbox = mailbox("a:Trash", role: "trash")
        model.messages = [m]
        model.selectedMessageIds = [m.id]

        model.deleteSelected()

        XCTAssertEqual(model.pendingPermanentDelete?.ids, [m.id], "confirmation requested")
        XCTAssertEqual(model.messages.map(\.id), [m.id], "row untouched until confirmed")
        XCTAssertNil(model.errorText, "nothing was attempted, so nothing can have failed")
    }

    func testCancellingTheConfirmationQueuesNothing() {
        let model = makeModel()
        model.selectedMailbox = mailbox("a:Trash", role: "trash")
        model.selectedMessageIds = ["a:Trash:1"]
        model.deleteSelected()

        model.cancelPermanentDelete()

        XCTAssertNil(model.pendingPermanentDelete)
        XCTAssertNil(model.errorText)
    }

    func testDeleteOutsideTrashNeverAsksForPermanentDeletion() {
        let model = makeModel()
        let m = header("a:INBOX:1", mailboxId: "a:INBOX")
        model.selectedMailbox = mailbox("a:INBOX", role: "inbox")
        model.messages = [m]
        model.selectedMessageIds = [m.id]

        model.deleteSelected()

        XCTAssertNil(model.pendingPermanentDelete, "Delete elsewhere still just moves to Trash")
    }

    func testConfirmWithNoPendingRequestDoesNothing() {
        let model = makeModel()
        model.confirmPermanentDelete()
        XCTAssertNil(model.errorText)
    }

    /// A failed permanent delete (here the core refuses an unknown message)
    /// must surface an error and leave the message in the list.
    func testFailedPermanentDeleteLeavesTheMessageVisible() async {
        let model = makeModel()
        let m = header("a:Trash:404", mailboxId: "a:Trash")
        model.selectedMailbox = mailbox("a:Trash", role: "trash")
        model.messages = [m]
        model.selectedMessageIds = [m.id]
        model.deleteSelected()

        model.confirmPermanentDelete()

        XCTAssertNil(model.pendingPermanentDelete, "the request is consumed")
        for _ in 0..<100 where model.errorText == nil {
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTAssertNotNil(model.errorText, "the failure must be visible")
        XCTAssertEqual(model.messages.map(\.id), [m.id], "message stays visible")
    }
}
