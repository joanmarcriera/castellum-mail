// Castellum - address autocomplete ranking (S47).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure merge/rank logic over two sources: locally-seen mail correspondents
// (Contact, from CastellumCore.listContacts) and macOS Contacts
// (SystemContactEntry, SystemContactsSource). No FFI, no Contacts.framework
// calls here — fully unit-testable without either.

import Foundation

/// One ranked suggestion for a To/Cc/Bcc completion.
struct AddressSuggestion: Equatable, Hashable {
    var name: String?
    var email: String

    /// The text a completion inserts: `"Name" <email>` (quoted when the name has
    /// RFC 5322 specials) or a bare email. The name comes from mail headers, so
    /// it is untrusted: control characters, quotes, backslashes and angle
    /// brackets are removed first, then `AddressFormatter` quotes what is left,
    /// so the compose parser always sees exactly ONE recipient.
    var completionText: String {
        AddressFormatter.header(name: Self.sanitizedName(name), email: email)
    }

    /// Display name with everything that could break out of the field removed.
    static func sanitizedName(_ name: String?) -> String? {
        guard let name else { return nil }
        let drop = CharacterSet.controlCharacters.union(CharacterSet(charactersIn: "\"<>\\"))
        let cleaned = String(String.UnicodeScalarView(name.unicodeScalars.filter { !drop.contains($0) }))
            .trimmingCharacters(in: .whitespaces)
        return cleaned.isEmpty ? nil : cleaned
    }

    /// True when `email` is safe to place inside `<...>` in the field: no
    /// whitespace, control characters or separators/brackets/quotes.
    static func isSafeAddress(_ email: String) -> Bool {
        let bad = CharacterSet.whitespacesAndNewlines.union(.controlCharacters)
            .union(CharacterSet(charactersIn: ",;<>\"\\"))
        return email.contains("@") && email.rangeOfCharacter(from: bad) == nil
    }
}

enum AddressCompleter {
    /// Ranked suggestions for `prefix`, merging mail-derived `localContacts`
    /// and `systemContacts`, deduplicated by lower-cased email (a local,
    /// mail-seen display name wins over a system-contacts one on a
    /// collision — it reflects how this person actually signs THEIR mail).
    /// Matches when the email OR the name CONTAINS `prefix`,
    /// case-insensitively (substring, not just prefix, so "smith" finds
    /// "John Smith" typed before any letter of "John").
    ///
    /// Ranking: (1) local (mail-seen) contacts before system-only ones — an
    /// address you have actually exchanged mail with is a better guess than
    /// one merely in your address book; (2) within that tier (which also holds `sentRecipients`, the
    /// addresses of mail we sent, counted double), by recency (`lastSeenAt` /
    /// `lastSentAt`, nil last); (3) then frequency (`inboundCount +
    /// 2 * outboundCount + 2 * sentCount`, descending). System-only entries are alphabetical by
    /// email after all local ones.
    static func suggestions(
        for prefix: String,
        localContacts: [Contact],
        systemContacts: [SystemContactEntry],
        sentRecipients: [SentRecipient] = [],
        limit: Int = 8
    ) -> [AddressSuggestion] {
        let needle = prefix.trimmingCharacters(in: .whitespaces).lowercased()
        guard !needle.isEmpty else { return [] }

        func matches(_ email: String, _ name: String?) -> Bool {
            email.lowercased().contains(needle) || (name?.lowercased().contains(needle) ?? false)
        }

        // Local tier: mail-seen contacts and recipients of mail we sent,
        // merged by lower-cased email. `sentRecipients` come from the
        // sent/outbox drafts, so an address just written to is known at once
        // (before the Sent copy syncs back). A sent message counts double: an
        // address we write to is a better guess than one that only writes us.
        struct Candidate {
            var name: String?
            var email: String
            var lastSeen: Int64
            var weight: Int64
        }
        var merged: [String: Candidate] = [:]
        for c in localContacts where matches(c.email, c.bestName) {
            merged[c.email.lowercased()] = Candidate(
                name: c.displayName ?? c.derivedName, email: c.email,
                lastSeen: c.lastSeenAt ?? -1, weight: c.inboundCount + 2 * c.outboundCount)
        }
        for r in sentRecipients where matches(r.email, r.name) {
            let key = r.email.lowercased()
            if var existing = merged[key] {
                existing.lastSeen = max(existing.lastSeen, r.lastSentAt)
                existing.weight += 2 * r.sentCount
                if existing.name == nil { existing.name = r.name }
                merged[key] = existing
            } else {
                merged[key] = Candidate(
                    name: r.name, email: r.email, lastSeen: r.lastSentAt, weight: 2 * r.sentCount)
            }
        }

        var seenEmails = Set<String>()
        var ranked: [AddressSuggestion] = []

        let local = merged.values.sorted { a, b in
            if a.lastSeen != b.lastSeen { return a.lastSeen > b.lastSeen }
            if a.weight != b.weight { return a.weight > b.weight }
            return a.email.lowercased() < b.email.lowercased() // stable across runs
        }
        for c in local {
            guard AddressSuggestion.isSafeAddress(c.email),
                  seenEmails.insert(c.email.lowercased()).inserted else { continue }
            ranked.append(AddressSuggestion(name: c.name, email: c.email))
        }

        let system = systemContacts
            .filter { matches($0.email, $0.name) }
            .sorted { $0.email.lowercased() < $1.email.lowercased() }
        for s in system {
            guard AddressSuggestion.isSafeAddress(s.email),
                  seenEmails.insert(s.email.lowercased()).inserted else { continue }
            ranked.append(AddressSuggestion(name: s.name, email: s.email))
        }

        return Array(ranked.prefix(limit))
    }
}
