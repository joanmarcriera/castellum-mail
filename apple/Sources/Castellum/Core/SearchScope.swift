// Castellum - search scope + header drill-down query building (MM-2,
//).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure functions behind the toolbar's scope menu and the reading pane's
// click-to-narrow header. The Rust core owns the grammar (search_query.rs,
// DECISIONS.md 2026-09-30 MM-2); this file only produces query TEXT in that
// grammar, so it is fully unit-tested without a core (SearchScopeTests).

import Foundation

/// Where a typed search looks. Persisted as a preference.
enum SearchScope: String, CaseIterable, Identifiable {
    case mailbox, account, all
    var id: String { rawValue }
    var label: String {
        switch self {
        case .mailbox: return String(localized: "Current mailbox")
        case .account: return String(localized: "Current account")
        case .all: return String(localized: "All accounts")
        }
    }
    var symbol: String {
        switch self {
        case .mailbox: return "folder"
        case .account: return "person.crop.circle"
        case .all: return "tray.2"
        }
    }
}

/// One concrete core call: `accountId == nil` = every account.
struct SearchPlan: Equatable {
    let accountId: String?
    let query: String
    /// Set when the search is narrowed to one mailbox (scope "Current
    /// mailbox"); only used to size the body-coverage note (#3633), the core
    /// narrows the search itself through the `path:` qualifier in `query`.
    var mailboxId: String? = nil
}

enum SearchQueryBuilder {
    /// A qualifier value in the grammar: bare when it is a single safe word,
    /// otherwise double-quoted. Inside quotes the core tokenizer reads `\"` as
    /// a literal quote and `\\` as a literal backslash, so any value
    /// round-trips (a subject like `Re: "Budget" v2` stays searchable).
    static func quote(_ value: String) -> String {
        let clean = value.trimmingCharacters(in: .whitespacesAndNewlines)
        let needsQuotes = clean.contains { $0.isWhitespace || $0 == ":" || $0 == "\"" }
            || clean.hasPrefix("-")
        guard needsQuotes else { return clean }
        let escaped = clean.replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        return "\"\(escaped)\""
    }

    static func token(_ qualifier: String, _ value: String) -> String {
        "\(qualifier):\(quote(value))"
    }

    /// Narrow `query` by one more qualifier (the reading-pane drill-down).
    /// A qualifier that must be unique (`date`, `within`) replaces an existing
    /// one; an identical token is never added twice.
    static func narrowing(_ query: String, qualifier: String, value: String) -> String {
        let new = token(qualifier, value)
        var parts = tokens(of: query)
        if parts.contains(new) { return parts.joined(separator: " ") }
        if singleValued.contains(qualifier.lowercased()) {
            parts.removeAll { $0.lowercased().hasPrefix(qualifier.lowercased() + ":") }
        }
        parts.append(new)
        return parts.joined(separator: " ")
    }

    private static let singleValued: Set<String> = ["date", "within"]

    /// Whitespace-split that keeps a quoted span (and its `key:` prefix,
    /// and a leading `-`) as one token — the same shape the core tokenizer
    /// reads.
    static func tokens(of query: String) -> [String] {
        var result: [String] = []
        var current = ""
        var inQuotes = false
        var escaped = false  // previous char was a backslash inside quotes
        for ch in query {
            if escaped { escaped = false; current.append(ch); continue }
            if inQuotes && ch == "\\" { escaped = true; current.append(ch); continue }
            if ch == "\"" { inQuotes.toggle(); current.append(ch); continue }
            if ch.isWhitespace && !inQuotes {
                if !current.isEmpty { result.append(current); current = "" }
            } else {
                current.append(ch)
            }
        }
        if !current.isEmpty { result.append(current) }
        return result
    }

    /// `date:YYYY-MM-DD` for an epoch second. The core reads `date:` as a UTC
    /// day, so the token is built from the UTC calendar day: the message you
    /// clicked is always inside the resulting range, wherever you are.
    static func dayToken(epoch: Int64) -> String {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(secondsFromGMT: 0)!
        let c = calendar.dateComponents([.year, .month, .day], from: Date(timeIntervalSince1970: TimeInterval(epoch)))
        return String(format: "%04d-%02d-%02d", c.year ?? 1970, c.month ?? 1, c.day ?? 1)
    }

    /// Resolves the typed text + scope + what is selected into one core call.
    /// A unified inbox or smart mailbox is inherently cross-account and
    /// ignores the scope, as before.
    static func plan(
        scope: SearchScope, text: String, selectedMailbox: Mailbox?,
        firstAccountId: String?, crossAccountSelection: Bool
    ) -> SearchPlan? {
        if crossAccountSelection || scope == .all {
            return SearchPlan(accountId: nil, query: text)
        }
        guard let account = selectedMailbox?.accountId ?? firstAccountId else { return nil }
        if scope == .mailbox, let mailbox = selectedMailbox {
            // `path:` is an exact match; `folder:` also matches a leaf name
            // and would pull in a same-named sub-folder.
            return SearchPlan(
                accountId: account, query: narrowing(text, qualifier: "path", value: mailbox.path),
                mailboxId: mailbox.id)
        }
        return SearchPlan(accountId: account, query: text)
    }
}
