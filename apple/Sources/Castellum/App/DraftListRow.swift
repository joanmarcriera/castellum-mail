// Castellum - DR1: what a draft looks like in a list (subject + recipient).
// Pure value types so the three draft shapes (local-only, server-saved, left
// behind by a refused send) are pinned by tests rather than by eye.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// One draft as a list row: its subject and WHO it is addressed to. In
/// Drafts and Outbox the recipient replaces the sender (always the account
/// owner there) — the one thing that tells two drafts apart.
struct DraftListRow: Equatable {
    static let noSubject = "(no subject)"
    static let noRecipient = "(no recipient)"

    let subject: String
    let recipient: String

    /// A local compose draft (any status: local-only, server-mirrored, or
    /// still sitting in `draft` after a refused send).
    init(draft: Draft) {
        self.subject = Self.subjectText(draft.subject)
        self.recipient = Self.recipientText(
            draft.to.map { Self.displayName(name: $0.name, email: $0.email) })
    }

    /// A message row in a Drafts-role mailbox (the server-saved copy).
    init(header: MessageHeader) {
        self.subject = Self.subjectText(header.subject ?? "")
        self.recipient = Self.recipientText(header.toDisplay.map { [$0] } ?? [])
    }

    static func subjectText(_ subject: String) -> String {
        subject.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? noSubject : subject
    }

    /// Names joined with ", "; the placeholder when no non-blank name remains.
    static func recipientText(_ names: [String]) -> String {
        let shown = names.filter { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
        return shown.isEmpty ? noRecipient : shown.joined(separator: ", ")
    }

    /// The display name when there is one, else the bare address.
    static func displayName(name: String?, email: String) -> String {
        let n = name?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return n.isEmpty ? email : n
    }
}

/// The text of the message list's "from" cell. Normal rows show the sender;
/// Drafts and Sent rows (those carrying `toDisplay`) show the recipient, so
/// the column is never a column of the account owner's own name.
func messageListSenderText(_ h: MessageHeader) -> String {
    if h.toDisplay != nil {
        return "To: " + DraftListRow(header: h).recipient
    }
    return h.fromName ?? h.fromAddress ?? "(unknown)"
}
