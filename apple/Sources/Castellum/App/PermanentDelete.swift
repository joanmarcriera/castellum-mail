// Castellum - "Delete inside Trash" policy (S73 /).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pressing Delete moves a message to Trash everywhere EXCEPT in the Trash
// mailbox itself, where it means "delete for good" — and only after the user
// has confirmed in a dialog that says it cannot be undone. This file holds
// the pure decision so it is unit-tested without a window; the destructive
// call itself lives in AppModel.confirmPermanentDelete.

import Foundation

enum DeleteDisposition: Equatable {
    /// Normal Delete: queue a move to Trash (undoable).
    case moveToTrash
    /// Delete inside Trash: ask first, then expunge permanently.
    case confirmPermanent

    /// Keyed on the mailbox ROLE (never its display name), matching the
    /// core's own check, which independently refuses a permanent delete of a
    /// message outside a role-`trash` mailbox.
    static func forMailboxRole(_ role: String?) -> DeleteDisposition {
        role == "trash" ? .confirmPermanent : .moveToTrash
    }

    /// Per-row rule for a selection: the confirmation only when the selection
    /// is non-empty and EVERY selected row sits in a role-`trash` mailbox.
    /// Mixed selections stay a (recoverable) move to Trash.
    static func forRowMailboxRoles(_ roles: [String?]) -> DeleteDisposition {
        guard !roles.isEmpty, roles.allSatisfy({ $0 == "trash" }) else { return .moveToTrash }
        return .confirmPermanent
    }
}

/// A permanent delete awaiting the user's confirmation. Holding it in
/// `AppModel.pendingPermanentDelete` is what presents the dialog; nothing is
/// queued until `confirmPermanentDelete()` consumes it.
struct PermanentDeleteRequest: Equatable {
    let ids: [String]

    var title: String {
        ids.count == 1
            ? String(localized: "Permanently delete this message?")
            : String(localized: "Permanently delete \(ids.count) messages?")
    }

    static var message: String {
        String(localized: "This can’t be undone. The message will be erased from the server.")
    }
}
