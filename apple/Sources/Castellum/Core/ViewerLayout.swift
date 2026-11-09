// Castellum - viewer layouts and the Correspondence view.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure data + functions behind View > Widescreen / Classic layout and the
// Correspondence bar. No UI, no core calls: everything here is unit-tested in
// ViewerLayoutTests. Thread Arcs and saved column sets are tracked separately
// (the tracker MM-4b) — they need a parent-link API from the core.

import Foundation

/// Where the message list sits relative to the reading pane.
enum ViewerLayout: String, CaseIterable, Identifiable {
    /// List on the left, reading pane on the right (Castellum's original layout).
    case widescreen
    /// List above, reading pane below (MailMate / Mail "classic").
    case classic

    var id: String { rawValue }
    var label: String {
        switch self {
        case .widescreen: return String(localized: "Widescreen (list beside message)")
        case .classic: return String(localized: "Classic (list above message)")
        }
    }
    static let defaultLayout: ViewerLayout = .widescreen
}

/// "All mail with this person" — the Correspondence view's subject.
struct CorrespondenceContext: Equatable {
    /// Lower-cased address that keys the view.
    let address: String
    let name: String?
    var title: String { name.map { "\($0) <\(address)>" } ?? address }
}

struct CorrespondenceStats: Equatable {
    var total = 0
    /// Sent by one of my own addresses / received from the other person.
    var sent = 0
    var received = 0
    var first: Date?
    var last: Date?
    /// Messages per calendar month (yyyy-MM in UTC), oldest first, for the
    /// last `monthsShown` months ending at the newest message.
    var perMonth: [(month: String, count: Int)] = []
    var busiestMonth: (month: String, count: Int)? { perMonth.max { $0.count < $1.count } }
    /// The searches hit their per-query cap: there is more mail than listed, so
    /// the totals and the first date are lower bounds ("500+").
    var isCapped = false

    static func == (a: Self, b: Self) -> Bool {
        a.total == b.total && a.sent == b.sent && a.received == b.received && a.isCapped == b.isCapped
            && a.first == b.first && a.last == b.last
            && a.perMonth.map(\.month) == b.perMonth.map(\.month)
            && a.perMonth.map(\.count) == b.perMonth.map(\.count)
    }
}

enum Correspondence {
    static let monthsShown = 12
    /// Rows fetched per from:/to:/cc: search.
    static let perQueryLimit: Int64 = 500

    /// Union of several result lists, de-duplicated by id, newest first (a
    /// message with no date sorts last; ties break by id for a stable order).
    static func merge(_ lists: [[MessageHeader]]) -> [MessageHeader] {
        var seen = Set<String>()
        var merged: [MessageHeader] = []
        for header in lists.joined() where seen.insert(header.id).inserted {
            merged.append(header)
        }
        return merged.sorted {
            ($0.sentAt ?? Int64.min, $1.id) > ($1.sentAt ?? Int64.min, $0.id)
        }
    }

    /// Merge for lists fetched with a per-query `cap`. A list that came back
    /// full (>= cap rows) is only known to be complete back to ITS oldest row;
    /// joining it with an uncapped list would show old mail from one source and
    /// silently miss it from the other. So the merged list is cut at the newest
    /// of the capped lists' oldest `sentAt`: everything shown is complete, and
    /// `capped` tells the UI the first date is a lower bound.
    static func mergeCapped(_ lists: [[MessageHeader]], cap: Int64) -> (messages: [MessageHeader], capped: Bool) {
        let merged = merge(lists)
        let cappedLists = lists.filter { Int64($0.count) >= cap }
        guard !cappedLists.isEmpty else { return (merged, false) }
        let cutoff = cappedLists
            .map { $0.map { $0.sentAt ?? Int64.min }.min() ?? Int64.min }
            .max() ?? Int64.min
        return (merged.filter { ($0.sentAt ?? Int64.min) >= cutoff }, true)
    }

    /// Every address that is "me": account addresses plus identity addresses,
    /// lower-cased.
    static func myAddresses(accountEmails: [String], identityEmails: [String]) -> Set<String> {
        Set((accountEmails + identityEmails).map { $0.lowercased() })
    }

    /// The three queries whose union is "everything between me and `address`":
    /// mail they sent, mail sent to them, mail copied to them.
    static func queries(for address: String) -> [String] {
        // `=` = this exact address (case-insensitive), not "contains", so
        // ann@x.test never also returns joann@x.test.
        let value = SearchQueryBuilder.quote("=" + address)
        return ["from:\(value)", "to:\(value)", "cc:\(value)"]
    }

    /// Whose correspondence "Show Correspondence" means for `detail`: the
    /// sender, unless the sender is one of my own addresses (a message I sent),
    /// in which case the first recipient who is not me. Nil when there is no one.
    static func otherParty(of detail: MessageDetail, myAddresses: Set<String>) -> (address: String, name: String?)? {
        let mine = Set(myAddresses.map { $0.lowercased() })
        if let from = detail.header.fromAddress, !mine.contains(from.lowercased()) {
            return (from, detail.header.fromName)
        }
        for kind in ["to", "cc", "bcc"] {
            if let other = detail.addresses.first(where: { $0.kind == kind && !mine.contains($0.address.lowercased()) }) {
                return (other.address, other.name)
            }
        }
        return nil
    }

    /// Counts for a merged correspondence list. `myAddresses` are my accounts'
    /// email addresses (case-insensitive): a message from one of them is
    /// "sent", any other is "received".
    static func stats(_ messages: [MessageHeader], myAddresses: Set<String>) -> CorrespondenceStats {
        var stats = CorrespondenceStats()
        let mine = Set(myAddresses.map { $0.lowercased() })
        var months: [String: Int] = [:]
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(secondsFromGMT: 0)!
        for m in messages {
            stats.total += 1
            if let from = m.fromAddress?.lowercased(), mine.contains(from) {
                stats.sent += 1
            } else {
                stats.received += 1
            }
            guard let sent = m.sentAt else { continue }
            let date = Date(timeIntervalSince1970: TimeInterval(sent))
            if stats.first.map({ date < $0 }) ?? true { stats.first = date }
            if stats.last.map({ date > $0 }) ?? true { stats.last = date }
            let c = calendar.dateComponents([.year, .month], from: date)
            months[String(format: "%04d-%02d", c.year ?? 1970, c.month ?? 1), default: 0] += 1
        }
        guard let newest = stats.last else { return stats }
        // The last `monthsShown` months ending at the newest message, zero-filled.
        var series: [(String, Int)] = []
        var cursor = newest
        for _ in 0..<monthsShown {
            let c = calendar.dateComponents([.year, .month], from: cursor)
            let key = String(format: "%04d-%02d", c.year ?? 1970, c.month ?? 1)
            series.append((key, months[key] ?? 0))
            cursor = calendar.date(byAdding: .month, value: -1, to: cursor) ?? cursor
        }
        stats.perMonth = series.reversed().map { (month: $0.0, count: $0.1) }
        return stats
    }
}
