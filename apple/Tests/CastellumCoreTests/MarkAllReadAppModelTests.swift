// Castellum - Tests for FP18: Mark All as Read never acts on a search result
// list, and says why when there is no single mailbox to act on.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class MarkAllReadAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-markall-\(UUID().uuidString)")
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

    private func unread(_ id: String) -> MessageHeader {
        MessageHeader(id: id, accountId: "a", mailboxId: "a:INBOX",
                      subject: nil, fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: false, isStarred: false, threadCount: 1)
    }

    private func inbox() -> Mailbox {
        Mailbox(id: "a:INBOX", accountId: "a", path: "INBOX", name: "INBOX", role: "inbox",
                delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    func testMarkAllWithAnActiveSearchLeavesRowsUnread() {
        let model = makeModel()
        model.selectedMailbox = inbox()
        model.searchText = "fox"
        model.messages = [unread("a:1"), unread("a:2")]
        model.markAllReadInCurrentMailbox()
        XCTAssertTrue(model.messages.allSatisfy { !$0.isRead }, "a search result list is not the mailbox")
    }

    func testMarkAllInAllInboxesExplainsInsteadOfDoingNothing() {
        let model = makeModel()
        model.selectedUnifiedRole = "inbox"
        model.messages = [unread("a:1")]
        model.markAllReadInCurrentMailbox()
        XCTAssertNotNil(model.errorText)
        XCTAssertTrue(model.messages.allSatisfy { !$0.isRead })
    }

    /// #3459: with an Inbox split tab on screen the core would mark the WHOLE Inbox read.
    func testMarkAllInASplitTabRefusesAndLeavesRowsUnread() throws {
        let model = makeModel()
        var scope = SmartMailboxScopeDTO()
        scope.inboxSplit = true
        let group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .subject, op: .contains, value: .text("news")),
        ])
        let req = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "News", query: "", symbol: nil, group: group, scope: scope)
        model.smartMailboxes = [try model.core.upsertSmartMailbox(
            json: try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(req)))]
        model.selectedMailbox = inbox()
        model.inboxSplitTabs = [InboxSplitCount(id: "x", name: "News", total: 1, unread: 1)]
        model.messages = [unread("a:1")]
        XCTAssertTrue(model.isInboxSplitStripVisible)

        model.markAllReadInCurrentMailbox()

        XCTAssertNotNil(model.errorText)
        XCTAssertTrue(model.messages.allSatisfy { !$0.isRead })
    }
}
