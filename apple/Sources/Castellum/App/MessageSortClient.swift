// Castellum - client-side message sort (, slice 4).
//
// The paged lists (a mailbox, All Inboxes) sort in the core. Every other list
// is fully loaded in memory (search results, smart mailboxes, Snoozed,
// Correspondence, Inbox split tabs), so a core round trip would buy nothing:
// they sort here, with the SAME ordering rules as `list_page.rs`:
//   - From / Subject: folded text (see `fold`, the Swift twin of the core's
//     `castellum_sort_fold`), ties newest first, then id; reversing the sort
//     reverses only the primary key;
//   - Received: undated rows are always last, ties by id (ascending: id desc,
//     as in the core, where the ascending segment is the exact reverse of the index);
//   - the flag keys (Unread, Flag, Attachments) group by their 0/1 value, then newest first.
// `MessageSortClientTests` carries the core's shared vector table, so a change
// to either fold is caught by the other suite.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum MessageSortClient {
    /// Reply/forward prefixes dropped from the front of a subject (lower-case, with the colon).
    private static let subjectPrefixes = ["re:", "fwd:", "fw:", "rv:", "ref:", "aw:"]

    /// Swift twin of the core's `sort_fold` (`native/crates/castellum-db/src/sort_fold.rs`):
    /// lower-case, combining marks dropped, Latin accented letters reduced to their base
    /// letter, trimmed; with `stripPrefixes` also repeated leading `re:`/`fwd:`/... removed.
    /// Foundation's `.diacriticInsensitive` is not used: it misses `đ ł ø`, which the core maps.
    static func fold(_ s: String, stripPrefixes: Bool) -> String {
        var out = String.UnicodeScalarView()
        for scalar in s.lowercased().unicodeScalars {
            if isCombiningMark(scalar) { continue }
            out.append(base(of: scalar))
        }
        var folded = String(out).trimmingCharacters(in: .whitespacesAndNewlines)
        if stripPrefixes {
            strip: while true {
                for p in subjectPrefixes where folded.hasPrefix(p) {
                    folded = String(folded.dropFirst(p.count))
                    folded = String(folded.drop(while: { $0.isWhitespace }))
                    continue strip
                }
                break
            }
        }
        return folded
    }

    private static func isCombiningMark(_ c: Unicode.Scalar) -> Bool {
        switch c.value {
        case 0x0300...0x036F, 0x1AB0...0x1AFF, 0x1DC0...0x1DFF, 0x20D0...0x20FF, 0xFE20...0xFE2F: return true
        default: return false
        }
    }

    /// The core's Latin base-letter table (`search_query::fold_text`), verbatim.
    private static let baseLetters: [Unicode.Scalar: Unicode.Scalar] = {
        let table: [(String, Character)] = [
            ("àáâãäåāăą", "a"), ("çćĉċč", "c"), ("ďđ", "d"), ("èéêëēĕėęě", "e"), ("ĝğġģ", "g"), ("ĥ", "h"),
            ("ìíîïĩīĭį", "i"), ("ĵ", "j"), ("ķ", "k"), ("ĺļľł", "l"), ("ñńņň", "n"), ("òóôõöøōŏő", "o"),
            ("ŕŗř", "r"), ("śŝşš", "s"), ("ţť", "t"), ("ùúûüũūŭůűų", "u"), ("ŵ", "w"), ("ýÿŷ", "y"), ("źżž", "z"),
        ]
        var map: [Unicode.Scalar: Unicode.Scalar] = [:]
        for (letters, baseChar) in table {
            for l in letters.unicodeScalars { map[l] = baseChar.unicodeScalars.first! }
        }
        return map
    }()

    private static func base(of c: Unicode.Scalar) -> Unicode.Scalar { baseLetters[c] ?? c }

    /// What the From column shows for sorting: the sender's name, else the address, else "".
    /// Matches the core's `COALESCE(NULLIF(trim(from_name), ''), from_address, '')`.
    static func senderKey(_ h: MessageHeader) -> String {
        let name = (h.fromName ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        return fold(name.isEmpty ? (h.fromAddress ?? "") : name, stripPrefixes: false)
    }

    static func subjectKey(_ h: MessageHeader) -> String { fold(h.subject ?? "", stripPrefixes: true) }

    /// `rows` in `sort` order. Stable and total: equal keys fall through to date (newest first)
    /// and then id, so the result is deterministic.
    static func sorted(_ rows: [MessageHeader], by sort: MessageSort) -> [MessageHeader] {
        switch sort.key {
        case .date:
            return rows.sorted { a, b in
                switch (a.sentAt, b.sentAt) {
                case (nil, nil): return a.id < b.id
                case (nil, _): return false          // undated rows last in BOTH directions
                case (_, nil): return true
                case let (x?, y?):
                    if x != y { return sort.ascending ? x < y : x > y }
                    return sort.ascending ? a.id > b.id : a.id < b.id
                }
            }
        case .unread, .starred, .attachments:
            func value(_ h: MessageHeader) -> Int {
                switch sort.key {
                case .unread: return h.isRead ? 1 : 0
                case .starred: return h.isStarred ? 1 : 0
                default: return h.hasAttachments ? 1 : 0
                }
            }
            return rows.sorted { a, b in
                let (va, vb) = (value(a), value(b))
                if va != vb { return sort.ascending ? va < vb : va > vb }
                let (da, db) = (a.sentAt ?? Int64.min, b.sentAt ?? Int64.min)
                if da != db { return da > db }
                return a.id < b.id
            }
        case .from, .subject:
            let keyed = rows.map { (row: $0, key: sort.key == .from ? senderKey($0) : subjectKey($0)) }
            return keyed.sorted { a, b in
                if a.key != b.key { return sort.ascending ? a.key < b.key : a.key > b.key }
                let (da, db) = (a.row.sentAt ?? Int64.min, b.row.sentAt ?? Int64.min)
                if da != db { return da > db }
                return a.row.id < b.row.id
            }.map(\.row)
        }
    }
}
