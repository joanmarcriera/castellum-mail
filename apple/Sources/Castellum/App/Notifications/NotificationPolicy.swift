// Castellum - the pure new-mail notification decision (S59).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Rust decides what is new (the notification outbox); this decides how to
// present a drained batch: which notices survive the user's mode, account
// exclusions and the Focus filter, and whether a burst collapses into one
// summary. Pure on purpose: no UNUserNotificationCenter, no clock, no
// UserDefaults. Every input is a parameter, so every branch is a unit test.

import Foundation

/// What to post for one drain.
enum NotificationPlan: Equatable {
    case message(PendingNotification, showPreview: Bool, sound: Bool)
    case summary(accountId: String, count: Int, firstOutboxId: Int64, sound: Bool)
}

enum NotificationPolicy {
    /// More than this many announceable messages for one account in one drain
    /// become a single summary ("12 new messages in Work").
    static let coalesceThreshold = 5

    static func plan(
        pending: [PendingNotification],
        mode: NewMailNotifications,
        excludedAccountIds: Set<String>,
        focus: FocusFilterState,
        showPreview: Bool,
        sound: Bool
    ) -> [NotificationPlan] {
        guard mode != .off else { return [] }
        let vipOnly = mode == .vipsOnly || focus.vipsOnly
        let kept = pending.filter { n in
            !excludedAccountIds.contains(n.accountId)
                && (focus.allowedAccountIds?.contains(n.accountId) ?? true)
                && (!vipOnly || n.fromVip)
        }
        // Keep drain order (outbox id order = arrival order) inside each account.
        var order: [String] = []
        var byAccount: [String: [PendingNotification]] = [:]
        for n in kept {
            if byAccount[n.accountId] == nil { order.append(n.accountId) }
            byAccount[n.accountId, default: []].append(n)
        }
        return order.flatMap { acct -> [NotificationPlan] in
            let items = byAccount[acct] ?? []
            if items.count > coalesceThreshold {
                return [
                    .summary(
                        accountId: acct, count: items.count,
                        firstOutboxId: items.first?.outboxId ?? 0, sound: sound)
                ]
            }
            return items.map { .message($0, showPreview: showPreview, sound: sound) }
        }
    }
}
