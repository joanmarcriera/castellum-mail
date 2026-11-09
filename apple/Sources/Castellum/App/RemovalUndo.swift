// Castellum - undo bookkeeping for Archive, Delete and Move.
//
// The core owns the durable side: every archive/delete/move is a queued `move`
// operation, and `castellum_undo_moves` queues its inverse (see
// `Database::undo_moves`). Swift owns presentation: this session-only stack of
// the last few removals, each registered with the window's `UndoManager` so
// Edit > Undo Archive / Cmd-Z works, and bound to the bare `z` key.
//
// The stack is a plain value type with no AppKit/FFI dependency so its
// ordering, cap and per-account pruning are unit-testable on their own
// (`RemovalUndoStackTests`).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// What the user did. Drives the Edit-menu action name ("Undo Archive").
enum RemovalVerb: Equatable, Sendable {
    case archive, delete, move, junk, notJunk

    /// Localised action name handed to `UndoManager.setActionName`; AppKit
    /// prefixes it with "Undo " in the Edit menu.
    var actionName: String {
        switch self {
        case .archive: return String(localized: "Archive")
        case .delete: return String(localized: "Delete")
        case .move: return String(localized: "Move")
        case .junk: return String(localized: "Mark as Junk")
        case .notJunk: return String(localized: "Not Junk")
        }
    }
}

/// Identity object an `UndoManager` registration targets. The manager keeps
/// only a weak reference, so dropping the stack entry (cap, account removal)
/// also retires the registration.
final class RemovalUndoToken {}

/// One undoable batch: the operation ids the core returned for it.
struct UndoableRemoval {
    let id = UUID()
    let verb: RemovalVerb
    let operationIds: [Int64]
    let count: Int
    let accountIds: Set<String>
    let token = RemovalUndoToken()
}

/// The last `capacity` undoable removals, newest last.
struct RemovalUndoStack {
    static let capacity = 20

    private(set) var entries: [UndoableRemoval] = []

    var isEmpty: Bool { entries.isEmpty }
    var depth: Int { entries.count }
    var top: UndoableRemoval? { entries.last }

    /// Push `entry`; returns entries evicted by the cap (oldest first) so the
    /// caller can retire their `UndoManager` registrations.
    @discardableResult
    mutating func push(_ entry: UndoableRemoval) -> [UndoableRemoval] {
        entries.append(entry)
        guard entries.count > Self.capacity else { return [] }
        let evicted = Array(entries.prefix(entries.count - Self.capacity))
        entries.removeFirst(evicted.count)
        return evicted
    }

    /// Remove and return the entry with `id` (nil when already gone, e.g. the
    /// banner and Cmd-Z both fired).
    mutating func remove(id: UUID) -> UndoableRemoval? {
        guard let index = entries.firstIndex(where: { $0.id == id }) else { return nil }
        return entries.remove(at: index)
    }

    /// Drop every entry that touches `accountId` (the account was removed, so
    /// its messages can never come back). Returns the dropped entries.
    @discardableResult
    mutating func dropEntries(forAccount accountId: String) -> [UndoableRemoval] {
        let dropped = entries.filter { $0.accountIds.contains(accountId) }
        entries.removeAll { $0.accountIds.contains(accountId) }
        return dropped
    }
}
