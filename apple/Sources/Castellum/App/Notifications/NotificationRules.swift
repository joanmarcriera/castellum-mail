// Castellum - counter scope and per-account notification rules (MM-9, slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure value types and the one function that turns "what to count" plus the
// numbers AppModel already holds into the text shown by the Dock badge and the
// menu bar counter. There is deliberately no second counting path: the inbox
// scope sums `model.counts` (what the sidebar's "All Inboxes" shows), the smart
// scope reads the sidebar's own `SmartMailbox.cachedUnread`, and the VIP scope
// is the existing `vipUnreadCount` FFI call.

import Foundation

/// What a counter (Dock badge or menu bar item) counts.
enum CounterScope: Equatable {
    case off
    case inboxUnread
    case vipUnread
    /// One smart mailbox, by id. An id that no longer exists falls back to inbox unread.
    case smartMailbox(id: String)

    /// Folds a stored mode and smart-mailbox id into a scope. A smart mode with
    /// no id chosen yet counts the inbox (never a silent zero).
    init(mode: DockBadgeMode, smartMailboxId: String?) {
        switch mode {
        case .off: self = .off
        case .inboxUnread: self = .inboxUnread
        case .vipUnread: self = .vipUnread
        case .smartMailbox:
            self = smartMailboxId.map { .smartMailbox(id: $0) } ?? .inboxUnread
        }
    }
}

/// Per-account notification settings, stored as JSON in `Preferences`. Slice 1
/// uses only `countsInCounters` (T7: one flag covers the Dock badge and the
/// menu bar); later slices add fields, so decoding tolerates missing keys.
struct AccountNotificationRule: Codable, Equatable {
    /// False = this account's mail is left out of the inbox and VIP counters.
    var countsInCounters: Bool = true

    init(countsInCounters: Bool = true) { self.countsInCounters = countsInCounters }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        countsInCounters = try c.decodeIfPresent(Bool.self, forKey: .countsInCounters) ?? true
    }
}

enum CounterLabel {
    /// Largest number shown exactly; above it the label reads "9999+".
    static let cap: Int64 = 9_999

    /// Accounts whose mail the inbox and VIP scopes include: the Focus filter's
    /// allowed set minus the accounts switched off in Settings. nil = every
    /// account (the cheap, unfiltered FFI call).
    static func includedAccountIds(
        allAccountIds: [String], focus: FocusFilterState, excluded: Set<String>
    ) -> [String]? {
        if focus.allowedAccountIds == nil && excluded.isEmpty { return nil }
        return allAccountIds.filter {
            (focus.allowedAccountIds?.contains($0) ?? true) && !excluded.contains($0)
        }
    }

    /// The text for a counter, nil when nothing should show (off, zero, or the
    /// Dock hidden by Focus). `honoursDockFlag` is true for the Dock badge only:
    /// Focus "Show Dock badge" is a Dock toggle and does not hide the menu bar item.
    static func text(
        scope: CounterScope, focus: FocusFilterState,
        inboxUnreadByAccount: [String: Int64], vipUnread: Int64?,
        smartMailboxes: [SmartMailbox], excludedAccountIds: Set<String> = [],
        honoursDockFlag: Bool = true
    ) -> String? {
        if honoursDockFlag && !focus.showDockBadge { return nil }
        func inbox() -> Int64 {
            inboxUnreadByAccount
                .filter { (focus.allowedAccountIds?.contains($0.key) ?? true) && !excludedAccountIds.contains($0.key) }
                .values.reduce(0, +)
        }
        let n: Int64
        switch scope {
        case .off:
            return nil
        case .inboxUnread:
            n = inbox()
        case .vipUnread:
            n = vipUnread ?? 0
        case .smartMailbox(let id):
            // The smart mailbox's own scope decides which accounts it covers, so
            // neither the Focus account limit nor the per-account flag applies
            // (stated in the Settings footnote).
            guard let sm = smartMailboxes.first(where: { $0.id == id }) else { n = inbox(); break }
            guard sm.cachedUnread <= cap else { return "\(cap)+" }
            return SmartMailboxBadge.label(unread: sm.cachedUnread, capped: sm.countsCapped)
        }
        guard n > 0 else { return nil }
        return n > cap ? "\(cap)+" : String(n)
    }
}
