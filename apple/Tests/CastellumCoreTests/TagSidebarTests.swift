// Castellum - MM-8 sidebar Tags section: badge text, TagCount decoding, and the
// AppModel selection rules for a tag's message list.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// AppModel runs against a temp profile; messages are written straight into the
// profile's castellum.db with DirectFixture (no IMAP), as in TagsAppModelTests.

import XCTest
@testable import Castellum

final class TagSidebarBadgeTests: XCTestCase {
    func testZeroUnreadHidesTheBadge() {
        XCTAssertNil(TagSidebarBadge.label(unread: 0))
    }

    func testUnreadShowsTheExactCount() {
        XCTAssertEqual(TagSidebarBadge.label(unread: 12), "12")
    }

    func testHelpMentionsLocalOnlyOnlyWhenThereAreSome() {
        XCTAssertFalse(TagSidebarBadge.help(total: 12, localOnly: 0).contains("this Mac"))
        let withLocal = TagSidebarBadge.help(total: 12, localOnly: 3)
        XCTAssertTrue(withLocal.contains("3") && withLocal.contains("12") && withLocal.contains("this Mac"), withLocal)
    }

    func testAccessibilityLabelNamesTheTagAndUnread() {
        XCTAssertEqual(TagSidebarBadge.accessibilityLabel(name: "Work", unread: 3), "Work, tag, 3 unread")
        XCTAssertEqual(TagSidebarBadge.accessibilityLabel(name: "Work", unread: 0), "Work, tag")
    }

    /// A core that predates `local_only` must still decode (the field defaults to 0).
    func testTagCountDecodesWithoutLocalOnly() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let old = try decoder.decode(
            TagCount.self, from: Data(#"{"keyword_key":"work","total":4,"unread":2}"#.utf8))
        XCTAssertEqual(old.localOnly, 0)
        let new = try decoder.decode(
            TagCount.self, from: Data(#"{"keyword_key":"work","total":4,"unread":2,"local_only":1}"#.utf8))
        XCTAssertEqual(new.localOnly, 1)
        XCTAssertEqual(new.total, 4)
    }
}

@MainActor
final class TagSidebarModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-tagsidebar-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 5,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline { try await Task.sleep(nanoseconds: 20_000_000) }
        if !condition() { XCTFail(message()) }
    }

    /// One account, one inbox, `count` messages ("a:INBOX:1"...), all unread.
    private func seed(_ model: AppModel, count: Int) throws -> (accountId: String, mailboxId: String, ids: [String]) {
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId,
            email: "a@example.test", mailboxId: mailboxId)
        var ids: [String] = []
        for n in 1...count {
            let id = "\(mailboxId):\(n)"
            try DirectFixture.seedMessage(
                profileDirectory: model.profileDirectory, id: id, accountId: accountId,
                mailboxId: mailboxId, subject: "Subject \(n)", fromAddress: "a@example.test",
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
            ids.append(id)
        }
        return (accountId, mailboxId, ids)
    }

    private func header(_ id: String, _ accountId: String, _ mailboxId: String) -> MessageHeader {
        MessageHeader(
            id: id, accountId: accountId, mailboxId: mailboxId, subject: "Test", fromName: nil,
            fromAddress: "a@example.test", sentAt: nil, snippet: nil, hasAttachments: false,
            hasBody: nil, isRead: false, isStarred: false, threadCount: 1)
    }

    func testSelectingATagClearsTheOtherListSelections() throws {
        let model = makeModel()
        let tag = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        model.selectedUnifiedRole = "inbox"
        model.selectedIsSnoozed = true
        model.selectedSmartMailboxId = "sm-1"
        model.select(tag: tag)
        XCTAssertEqual(model.selectedTagKey, "work")
        XCTAssertNil(model.selectedUnifiedRole)
        XCTAssertNil(model.selectedMailbox)
        XCTAssertNil(model.selectedSmartMailboxId)
        XCTAssertFalse(model.selectedIsSnoozed)
        XCTAssertEqual(model.searchText, "")
        XCTAssertTrue(model.isCrossAccountList)
    }

    func testSelectingAnotherListLeavesTheTag() throws {
        let model = makeModel()
        let tag = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        model.select(tag: tag)
        model.select(unifiedRole: "inbox")
        XCTAssertNil(model.selectedTagKey)
        model.select(tag: tag)
        model.select(snoozed: ())
        XCTAssertNil(model.selectedTagKey)
    }

    func testTagListShowsTheTaggedMessagesAndCountsAgree() async throws {
        let model = makeModel()
        let s = try seed(model, count: 3)
        let tag = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        _ = try model.core.setMessageTag(messageId: s.ids[0], keyword: "Work", add: true)
        _ = try model.core.setMessageTag(messageId: s.ids[2], keyword: "Work", add: true)
        model.select(tag: tag)
        try await waitUntil({ model.messages.count == 2 }, "tag list did not load")
        XCTAssertEqual(Set(model.messages.map(\.id)), [s.ids[0], s.ids[2]])
        model.reloadTagCounts()
        try await waitUntil({ model.tagCounts["work"]?.total == 2 }, "counts did not load")
        XCTAssertEqual(model.tagCounts["work"]?.unread, 2)
        XCTAssertEqual(model.tagCounts["work"]?.localOnly, 2, "evaluation-mode tags are local-only")
    }

    func testCountsRefreshAfterToggleTag() async throws {
        let model = makeModel()
        let s = try seed(model, count: 1)
        _ = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        model.messages = [header(s.ids[0], s.accountId, s.mailboxId)]
        model.selectedMessageIds = [s.ids[0]]
        model.toggleTag("Work")
        try await waitUntil({ model.tagCounts["work"]?.total == 1 }, "badge did not follow the tag")
        model.toggleTag("Work")
        try await waitUntil({ model.tagCounts["work"]?.total == 0 }, "badge did not follow the untag")
    }

    func testDeletingTheSelectedTagFallsBackToTheDefaultList() throws {
        let model = makeModel()
        let tag = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        model.select(tag: tag)
        model.deleteTag(tag)
        XCTAssertNil(model.selectedTagKey)
        XCTAssertNil(model.tagCounts["work"])
        XCTAssertFalse(model.tags.contains { $0.keywordKey == "work" })
    }

    /// Keyset paging: the next page starts after the last row shown, so rows that
    /// disappear from the loaded list meanwhile never shift it (no skip, no repeat).
    func testKeysetPagesContinueAfterTheLastRowEvenIfEarlierRowsVanish() throws {
        let model = makeModel()
        let s = try seed(model, count: 5)
        _ = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        for id in s.ids { _ = try model.core.setMessageTag(messageId: id, keyword: "Work", add: true) }
        let first = try model.core.messagesForTag(keyword: "Work", limit: 2)
        XCTAssertEqual(first.count, 2)
        // Untag the first row of page one (what a local archive/delete or untag does).
        _ = try model.core.setMessageTag(messageId: first[0].id, keyword: "Work", add: false)
        let second = try model.core.messagesForTag(keyword: "Work", limit: 2, after: first.last)
        let third = try model.core.messagesForTag(keyword: "Work", limit: 2, after: second.last)
        let all = [first[1]] + second + third
        XCTAssertEqual(all.count, 4)
        XCTAssertEqual(Set(all.map(\.id)).count, 4, "no row repeats")
        XCTAssertEqual(Set(all.map(\.id)), Set(s.ids).subtracting([first[0].id]), "no row is skipped")
    }

    /// A page requested while a refresh is in flight carries the OLD last row;
    /// when new tagged mail shifts the refreshed boundary, appending that page
    /// would skip the displaced rows. The refresh drops it on landing.
    func testRefreshThatShiftsTheBoundaryLeavesNoGapBetweenPages() async throws {
        let model = makeModel()
        let tag = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId,
            email: "a@example.test", mailboxId: mailboxId)
        func add(_ n: Int, sentAt: Int64) throws -> String {
            let id = "\(mailboxId):\(n)"
            try DirectFixture.seedMessage(
                profileDirectory: model.profileDirectory, id: id, accountId: accountId,
                mailboxId: mailboxId, subject: "Subject \(n)", fromAddress: "a@example.test",
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false,
                sentAt: sentAt)
            _ = try model.core.setMessageTag(messageId: id, keyword: "Work", add: true)
            return id
        }
        var all: [String] = []
        for n in 0..<600 { all.append(try add(n, sentAt: 1_700_000_000 + Int64(n))) }
        model.select(tag: tag)
        try await waitUntil({ model.messages.count == 500 }, "first page did not load")
        // New tagged mail arrives at the top, then a sync refresh and a scroll race.
        let arrival = try add(600, sentAt: 1_800_000_000)
        all.append(arrival)
        model.reloadCurrentMessages()
        model.loadMoreMessagesIfNeeded(lastVisibleRow: 450)
        try await waitUntil({
            model.loadMoreMessagesIfNeeded(lastVisibleRow: model.messages.count)
            return model.messages.first?.id == arrival && !model.hasMoreMessages
        }, timeout: 10, "list did not settle")
        let ids = model.messages.map(\.id)
        XCTAssertEqual(Set(ids).count, ids.count, "no row listed twice")
        XCTAssertEqual(Set(ids), Set(all), "no row skipped between pages")
    }

    func testTagListDoesNotOfferColumnSorting() throws {
        let model = makeModel()
        let tag = try XCTUnwrap(model.createTag(displayName: "Work", colour: "blue"))
        model.select(tag: tag)
        XCTAssertTrue(model.sortableColumnIds.isEmpty)
        XCTAssertNil(model.displaySort)
    }
}
