// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - Split Inbox (HP-2): the scope flag's JSON contract, the tab
// badge, and a full-stack (Swift -> FFI -> Rust) split that survives reopening
// the profile. The partition itself (exactly one tab per message, counts equal
// lists) is pinned by the Rust tests in castellum-db/src/inbox_splits.rs.
import XCTest
@testable import Castellum

final class InboxSplitTests: XCTestCase {
    private func tempProfile() -> URL {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-swift-inbox-splits-\(UUID().uuidString)")
    }

    private func decodeScope(_ json: String) throws -> SmartMailboxScopeDTO {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(SmartMailboxScopeDTO.self, from: Data(json.utf8))
    }

    func testScopeWrittenBeforeSplitsDecodesAsNotASplit() throws {
        let old = try decodeScope(#"{"account_ids":["a"],"mailbox_ids":[]}"#)
        XCTAssertEqual(old.accountIds, ["a"])
        XCTAssertFalse(old.inboxSplit)
        XCTAssertTrue(try decodeScope(#"{"account_ids":[],"mailbox_ids":[],"inbox_split":true}"#).inboxSplit)
        XCTAssertFalse(try decodeScope("{}").inboxSplit)
    }

    func testScopeEncodesTheFlagForTheCore() throws {
        var scope = SmartMailboxScopeDTO()
        scope.inboxSplit = true
        let json = String(decoding: try CastellumCore.encoder.encode(scope), as: UTF8.self)
        XCTAssertTrue(json.contains(#""inbox_split":true"#), json)
    }

    func testBadgeHidesZeroAndShowsUnread() {
        XCTAssertNil(InboxSplitBadge.label(unread: 0))
        XCTAssertEqual(InboxSplitBadge.label(unread: 7), "7")
    }

    func testOtherTabIsLocalisedByIdNotByName() {
        let other = InboxSplitCount(id: "other", name: "Other", total: 3, unread: 1)
        let split = InboxSplitCount(id: "x", name: "Invoices", total: 1, unread: 0)
        XCTAssertTrue(other.isOther)
        XCTAssertFalse(split.isOther)
        XCTAssertEqual(split.displayName, "Invoices")
        XCTAssertFalse(other.displayName.isEmpty)
    }

    /// A split built with the editor's DTOs is stored by the real core, shows
    /// up as a tab (then Other, last) and is still a split after reopening.
    func testSplitRoundTripsThroughTheCoreAndSurvivesReopening() throws {
        let dir = tempProfile()
        let core = try CastellumCore(profilePath: dir)
        let group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .subject, op: .contains, value: .text("invoice")),
        ])
        var scope = SmartMailboxScopeDTO()
        scope.inboxSplit = true
        let req = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "Invoices", query: "", symbol: nil, group: group, scope: scope)
        let saved = try core.upsertSmartMailbox(json: try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(req)))
        XCTAssertTrue(saved.isInboxSplit)

        let tabs = try core.inboxSplitCounts(mailboxIds: [])
        XCTAssertEqual(tabs.map(\.id), [saved.id, InboxSplitCount.otherId])
        XCTAssertEqual(tabs.map(\.total), [0, 0], "no mail seeded")
        XCTAssertEqual(try core.inboxSplitMessages(mailboxIds: [], splitId: saved.id, limit: 10)?.count, 0)
        XCTAssertNil(try core.inboxSplitMessages(mailboxIds: [], splitId: "gone", limit: 10))

        let reopened = try CastellumCore(profilePath: dir)
        let listed = try XCTUnwrap(try reopened.listSmartMailboxes().first)
        XCTAssertTrue(listed.isInboxSplit)
        XCTAssertEqual(try reopened.inboxSplitCounts(mailboxIds: []).first?.name, "Invoices")

        // A plain group mailbox is not a tab.
        let plain = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "Not a tab", query: "", symbol: nil, group: group, scope: nil)
        let notSplit = try reopened.upsertSmartMailbox(json: try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(plain)))
        XCTAssertFalse(notSplit.isInboxSplit)
        XCTAssertEqual(try reopened.inboxSplitCounts(mailboxIds: []).count, 2)
    }

    // MARK: failure fallback (#3441)

    /// If the split counts/rows cannot load, the Inbox must show its plain page, not the
    /// previous mailbox's rows, and stay out of split mode until the next selection.
    @MainActor
    func testSplitFailureFallsBackToThePlainInboxPage() async throws {
        let dir = tempProfile()
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: accountId, email: "a@example.test",
            mailboxId: mailboxId, role: "inbox")
        try DirectFixture.seedMessage(
            profileDirectory: dir, id: "\(mailboxId):1", accountId: accountId, mailboxId: mailboxId,
            subject: "Hello", fromAddress: "b@example.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)

        var scope = SmartMailboxScopeDTO()
        scope.inboxSplit = true
        let group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .subject, op: .contains, value: .text("zzz")),
        ])
        let req = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "Z", query: "", symbol: nil, group: group, scope: scope)
        let saved = try model.core.upsertSmartMailbox(json: try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(req)))
        model.smartMailboxes = [saved]
        model.messages = [MessageHeader(
            id: "stale", accountId: "other", mailboxId: "other:Sent", subject: "Sent mail",
            fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false,
            hasBody: nil, isRead: true, isStarred: false, threadCount: 1)]

        model.select(unifiedRole: "inbox")
        XCTAssertTrue(model.isInboxSplitActive)
        model.fallBackFromInboxSplit()
        XCTAssertFalse(model.isInboxSplitActive, "split mode is off after a failure")
        XCTAssertTrue(model.inboxSplitTabs.isEmpty)

        let deadline = Date().addingTimeInterval(5)
        while model.messages.map(\.id) != ["\(mailboxId):1"], Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertEqual(model.messages.map(\.id), ["\(mailboxId):1"], "the plain Inbox page replaced the stale rows")

        model.select(unifiedRole: "inbox")  // the next selection tries splits again
        XCTAssertTrue(model.isInboxSplitActive)
    }
}
