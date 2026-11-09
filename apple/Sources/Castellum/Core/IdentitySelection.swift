// Castellum - choosing the From identity for a new compose window (MM-6,
//).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure, unit-tested (IdentitySelectionTests). Order, first hit wins:
//   1. a REPLY to a message that was addressed to one of my addresses uses that
//      address — To before Cc, in the order the original listed them. For each
//      address, an identity with exactly that address comes first (so a
//      plus-address identity like me+work@host is chosen for mail sent to it);
//      only then does the account's own address, or a plus-tagged form of it
//      no identity claims, count as "own". Own (i.e. no alias) wins over the
//      default alias: answering as the address you were written to matters
//      more than the "default for new mail" setting;
//   2. otherwise the account's default identity (the one flagged "default"),
//   3. otherwise the account's own address (nil = no alias).
// A forward and a brand-new message have no "addressed to" and use 2 then 3.
// Once the user picks a From by hand, or a draft is reopened with its stored
// From, this never runs again for that window.

import Foundation

/// What the compose window was opened for.
enum IdentitySelectionContext: Equatable {
    case newMessage
    /// A reply / reply-all; `recipients` = the original's To then Cc addresses.
    case reply(recipients: [String])
}

enum IdentitySelector {
    /// The identity to start with, or nil for the account's own address.
    static func choose(
        context: IdentitySelectionContext, accountEmail: String, identities: [Identity]
    ) -> ComposeIdentity? {
        if case .reply(let recipients) = context {
            let own = accountEmail.lowercased()
            for address in recipients.map({ $0.lowercased() }) {
                // An identity with exactly this address wins FIRST (review of
                // PR #206): a plus-address identity (me+work@host on account
                // me@host) must be choosable, so the plus-tag fallback below
                // may only apply when no identity owns the address itself.
                if let match = identities.first(where: { $0.email.lowercased() == address }) {
                    return ComposeIdentity(email: match.email, name: match.displayName)
                }
                // Otherwise me+shop@host is still the account's own mailbox.
                if address == own || dropPlusTag(address) == own { return nil }
            }
        }
        if let preferred = identities.first(where: \.isDefault) {
            return ComposeIdentity(email: preferred.email, name: preferred.displayName)
        }
        return nil
    }
}

/// `me+tag@host` -> `me@host` (a plus tag never changes who the mailbox is).
func dropPlusTag(_ address: String) -> String {
    guard let at = address.lastIndex(of: "@"),
          let plus = address[..<at].firstIndex(of: "+") else { return address }
    return String(address[..<plus]) + String(address[at...])
}

/// Validation for the identity sheet's Reply-To and Bcc fields — the core
/// re-checks, this only stops Save from being offered for a bad value.
enum IdentityAddressField {
    /// Empty is fine (it clears); otherwise exactly one plain address.
    static func isAcceptable(_ text: String) -> Bool {
        let value = text.trimmingCharacters(in: .whitespaces)
        if value.isEmpty { return true }
        return AddressParser.isValidEmail(value)
            && !value.contains(where: { $0 == "," || $0 == ";" || $0 == "<" || $0 == ">" || $0 == "\"" })
    }
}
