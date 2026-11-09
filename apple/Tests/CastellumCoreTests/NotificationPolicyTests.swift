// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// The pure notification decision (S59): every branch of `NotificationPolicy`
/// with no notification centre, clock or UserDefaults involved.
final class NotificationPolicyTests: XCTestCase {
    private func notice(
        _ id: Int64, account: String = "a", vip: Bool = false
    ) -> PendingNotification {
        PendingNotification(
            outboxId: id, accountId: account, messageId: "\(account):INBOX:\(id)",
            mailboxId: "\(account):INBOX", threadId: "t\(id)", fromName: "Ana",
            fromAddress: "ana@example.com", subject: "Subject \(id)", snippet: "Snippet \(id)",
            sentAt: 1_800_000_000, fromVip: vip)
    }

    private func plan(
        _ pending: [PendingNotification],
        mode: NewMailNotifications = .all,
        excluded: Set<String> = [],
        focus: FocusFilterState = .inactive,
        preview: Bool = true,
        sound: Bool = true
    ) -> [NotificationPlan] {
        NotificationPolicy.plan(
            pending: pending, mode: mode, excludedAccountIds: excluded, focus: focus,
            showPreview: preview, sound: sound)
    }

    func testOffModePostsNothing() {
        XCTAssertEqual(plan([notice(1), notice(2, vip: true)], mode: .off), [])
    }

    func testExcludedAccountIsDropped() {
        let out = plan([notice(1, account: "a"), notice(2, account: "b")], excluded: ["a"])
        XCTAssertEqual(out, [.message(notice(2, account: "b"), showPreview: true, sound: true)])
    }

    func testFocusAllowedSetDropsOtherAccountsAndNilAllowsAll() {
        let both = [notice(1, account: "a"), notice(2, account: "b")]
        let onlyB = FocusFilterState(allowedAccountIds: ["b"], vipsOnly: false, showDockBadge: true)
        XCTAssertEqual(
            plan(both, focus: onlyB),
            [.message(notice(2, account: "b"), showPreview: true, sound: true)])
        XCTAssertEqual(plan(both, focus: .inactive).count, 2)
        // An empty allowed set is "no account", not "all".
        let none = FocusFilterState(allowedAccountIds: [], vipsOnly: false, showDockBadge: true)
        XCTAssertEqual(plan(both, focus: none), [])
    }

    func testVipsOnlyModeAndFocusVipsOnlyEachKeepOnlyVips() {
        let mixed = [notice(1, vip: false), notice(2, vip: true)]
        let expected: [NotificationPlan] = [.message(notice(2, vip: true), showPreview: true, sound: true)]
        XCTAssertEqual(plan(mixed, mode: .vipsOnly), expected)
        let focusVips = FocusFilterState(allowedAccountIds: nil, vipsOnly: true, showDockBadge: true)
        XCTAssertEqual(plan(mixed, focus: focusVips), expected)
    }

    func testSixMessagesForOneAccountBecomeOneSummaryFiveStayIndividual() {
        let six = (1...6).map { notice(Int64($0)) }
        XCTAssertEqual(
            plan(six), [.summary(accountId: "a", count: 6, firstOutboxId: 1, sound: true)])
        let five = (1...5).map { notice(Int64($0)) }
        XCTAssertEqual(plan(five).count, 5)
        XCTAssertTrue(plan(five).allSatisfy { if case .message = $0 { return true } else { return false } })
    }

    func testTwoAccountsKeepFirstSeenOrderAndCoalesceIndependently() {
        var pending = [notice(1, account: "b")]
        pending += (2...7).map { notice(Int64($0), account: "a") }
        pending.append(notice(8, account: "b"))
        let out = plan(pending)
        XCTAssertEqual(out.count, 3)
        XCTAssertEqual(out[0], .message(notice(1, account: "b"), showPreview: true, sound: true))
        XCTAssertEqual(out[1], .message(notice(8, account: "b"), showPreview: true, sound: true))
        XCTAssertEqual(out[2], .summary(accountId: "a", count: 6, firstOutboxId: 2, sound: true))
    }

    func testPreviewAndSoundFlagsPassThrough() {
        XCTAssertEqual(
            plan([notice(1)], preview: false, sound: false),
            [.message(notice(1), showPreview: false, sound: false)])
    }

    func testMessageHeaderJSONWithoutFromVipDecodesToFalse() throws {
        let json = """
            {"id":"a:INBOX:1","account_id":"a","mailbox_id":"a:INBOX","subject":"s",
             "from_name":null,"from_address":"x@y.z","sent_at":1,"snippet":null,
             "has_attachments":false,"has_body":true,"is_read":false,"is_starred":false,
             "thread_count":1}
            """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let header = try decoder.decode(MessageHeader.self, from: Data(json.utf8))
        XCTAssertFalse(header.fromVip)
        let vip = try decoder.decode(
            MessageHeader.self, from: Data(json.replacingOccurrences(of: "\"thread_count\":1", with: "\"thread_count\":1,\"from_vip\":true").utf8))
        XCTAssertTrue(vip.fromVip)
    }

    func testPendingNotificationDecodesFromCoreJSON() throws {
        let json = """
            [{"outbox_id":7,"account_id":"a","message_id":"a:INBOX:1","mailbox_id":"a:INBOX",
              "thread_id":null,"from_name":"Ana","from_address":"ana@example.com",
              "subject":"Hi","snippet":"There","sent_at":5,"from_vip":true}]
            """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let got = try decoder.decode([PendingNotification].self, from: Data(json.utf8))
        XCTAssertEqual(got.first?.outboxId, 7)
        XCTAssertEqual(got.first?.fromVip, true)
        XCTAssertNil(got.first?.threadId)
    }
}
