// Castellum - S34 "All Inboxes" unread-badge aggregation (pure helper).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class UnifiedInboxBadgeTests: XCTestCase {
    private func mb(_ id: String, accountId: String, role: String) -> Mailbox {
        Mailbox(id: id, accountId: accountId, path: id, name: id, role: role,
                delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    func testSumsUnreadAcrossAccountsForTheGivenRoleOnly() {
        let mailboxes: [String: [Mailbox]] = [
            "a": [mb("a:INBOX", accountId: "a", role: "inbox"), mb("a:Sent", accountId: "a", role: "sent")],
            "b": [mb("b:INBOX", accountId: "b", role: "inbox")],
        ]
        let counts: [String: MailboxCounts] = [
            "a:INBOX": MailboxCounts(mailboxId: "a:INBOX", total: 10, unread: 3),
            "a:Sent": MailboxCounts(mailboxId: "a:Sent", total: 5, unread: 5),
            "b:INBOX": MailboxCounts(mailboxId: "b:INBOX", total: 2, unread: 1),
        ]
        XCTAssertEqual(UnifiedInboxBadge.unreadCount(role: "inbox", mailboxes: mailboxes, counts: counts), 4)
    }

    func testMissingCountsCountAsZeroNotACrash() {
        let mailboxes: [String: [Mailbox]] = ["a": [mb("a:INBOX", accountId: "a", role: "inbox")]]
        XCTAssertEqual(UnifiedInboxBadge.unreadCount(role: "inbox", mailboxes: mailboxes, counts: [:]), 0)
    }
}
