// Castellum - the "Archived 3 messages · Undo" banner (, slice 2).
//
// Same discipline as UndoSendBanner.swift: `RemovalUndoBannerState` is pure (no
// SwiftUI, no clock) so its transitions are unit-tested directly
// (`RemovalUndoBannerTests`); `RemovalUndoBannerView` is the thin non-modal
// presentation mounted in RootView's bottom inset.
//
// Two phases for the entry currently shown:
//   - offering:  "Archived 3 messages · Undo", 8 s, then the banner hides (the
//     stack entry stays, so Cmd-Z / bare `z` still work);
//   - returning: after Undo, "Returning to Inbox…" until the post-replay sync
//     has landed (or 30 s pass).
//
// The Undo button deliberately has NO keyboard shortcut: the undo-send banner
// binds Cmd-Z while it is visible (its window is shorter and more urgent), so
// removal undo is reached through Edit > Undo or bare `z`.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct RemovalUndoBannerState: Equatable {
    static let offerSeconds = 8
    static let returningSeconds = 30

    enum Phase: Equatable {
        /// Undo is on offer; `remaining` seconds until the banner hides.
        case offering(remaining: Int)
        /// Undo was requested. `replayed` flips once the move back reached the
        /// server, after which the next completed sync of the account clears it.
        case returning(remaining: Int, replayed: Bool)
    }

    struct Shown: Equatable {
        let entryId: UUID
        let verb: RemovalVerb
        let count: Int
        let accountIds: Set<String>
        var phase: Phase
        /// Folder name for "Returning to %@…", known once Undo is requested.
        var destinationName: String?
    }

    private(set) var shown: Shown?

    var isVisible: Bool { shown != nil }

    /// A removal was just recorded. Replaces any earlier banner (the earlier
    /// entry stays undoable through Cmd-Z / `z`).
    mutating func offer(entryId: UUID, verb: RemovalVerb, count: Int, accountIds: Set<String>) {
        guard count > 0 else { return }
        shown = Shown(
            entryId: entryId, verb: verb, count: count, accountIds: accountIds,
            phase: .offering(remaining: Self.offerSeconds), destinationName: nil)
    }

    /// One second elapsed. Hides the banner when its phase runs out.
    mutating func tick() {
        guard var s = shown else { return }
        switch s.phase {
        case .offering(let remaining):
            if remaining <= 1 { shown = nil; return }
            s.phase = .offering(remaining: remaining - 1)
        case .returning(let remaining, let replayed):
            if remaining <= 1 { shown = nil; return }
            s.phase = .returning(remaining: remaining - 1, replayed: replayed)
        }
        shown = s
    }

    /// Undo was requested for `entryId` (banner button, Cmd-Z or `z`). If the
    /// banner is showing that entry it switches to "Returning…"; for an older
    /// entry the banner is left alone.
    mutating func undoRequested(entryId: UUID, destinationName: String?) {
        guard var s = shown, s.entryId == entryId else { return }
        s.phase = .returning(remaining: Self.returningSeconds, replayed: false)
        s.destinationName = destinationName
        shown = s
    }

    /// The core replayed operations for these accounts (the move back reached
    /// the server). The next completed sync then clears "Returning…".
    mutating func replayApplied(accountIds: [String]) {
        guard var s = shown, case .returning(let remaining, _) = s.phase,
              !s.accountIds.isDisjoint(with: accountIds) else { return }
        s.phase = .returning(remaining: remaining, replayed: true)
        shown = s
    }

    /// A sync of `accountId` finished and the list was reloaded.
    mutating func syncCompleted(accountId: String) {
        guard let s = shown, case .returning(_, true) = s.phase,
              s.accountIds.contains(accountId) else { return }
        shown = nil
    }

    /// Undo was refused or the entry vanished: drop the banner if it is `entryId`'s.
    mutating func clear(entryId: UUID) {
        if shown?.entryId == entryId { shown = nil }
    }

    mutating func dismiss() { shown = nil }

    // MARK: - Text

    /// "Archived 3 messages" / "Moved 1 message to Trash" / "Moved 2 messages".
    /// `bundle` is a test seam for the CA/ES lproj bundles.
    static func offerText(verb: RemovalVerb, count: Int, bundle: Bundle = .main) -> String {
        switch verb {
        case .archive:
            return String(localized: "Archived \(count) messages", bundle: bundle)
        case .delete:
            return String(localized: "Moved \(count) messages to Trash", bundle: bundle)
        case .move:
            return String(localized: "Moved \(count) messages", bundle: bundle)
        case .junk:
            return String(localized: "Moved \(count) messages to Junk", bundle: bundle)
        case .notJunk:
            return String(localized: "Moved \(count) messages out of Junk", bundle: bundle)
        }
    }

    static func returningText(destination: String?, bundle: Bundle = .main) -> String {
        let name = destination ?? String(localized: "your mailbox", bundle: bundle)
        return String(localized: "Returning to \(name)…", bundle: bundle)
    }
}

/// The non-modal banner. Presentation only; state lives in `AppModel.removalUndoBanner`.
struct RemovalUndoBannerView: View {
    let shown: RemovalUndoBannerState.Shown
    let onUndo: () -> Void
    let onDismiss: () -> Void

    private var text: String {
        switch shown.phase {
        case .offering:
            return RemovalUndoBannerState.offerText(verb: shown.verb, count: shown.count)
        case .returning:
            return RemovalUndoBannerState.returningText(destination: shown.destinationName)
        }
    }

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: shown.verb == .delete ? "trash" : "archivebox")
                .foregroundStyle(.secondary)
            Text(text)
            Spacer()
            if case .offering = shown.phase {
                // No keyboard shortcut on purpose: see the file header.
                Button("Undo", action: onUndo)
            }
            Button {
                onDismiss()
            } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Dismiss")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.bar)
        .overlay(alignment: .top) { Divider() }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(text)
    }
}
