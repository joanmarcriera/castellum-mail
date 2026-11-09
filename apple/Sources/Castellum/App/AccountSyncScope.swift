// Castellum - first-sync scope: which folders, how much history (S62).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// After a successful Test Connection the setup sheet lists the server's
// folders (pre-ticked with exactly the set the core would sync by default)
// and offers an initial-history window, so a first sync against a large
// mailbox isn't "everything, blind". Pure and unit-tested; the sheet only
// binds state to it.
//
// Two deliberate choices:
//   * Leaving the default folders ticked stores NO filter (`mailboxes` nil),
//     not the equivalent explicit list — the core's default policy also
//     picks up folders created or subscribed later, an explicit list never
//     would. Only a real deviation from the defaults is persisted.
//   * History is stored as an absolute date computed at setup time, not
//     "N months" — the local store then consistently means "everything
//     since D", including for folders first synced later. (A UIDVALIDITY
//     reset re-imports in full instead, so mail already held isn't lost.)
//
// (S62 residual): the SAME policy functions also back
// Settings > Accounts > "Folders…", which edits an EXISTING account's
// selection after setup. `currentSelection`/`resolvedKeepPaths` below are
// the only additions that flow needs — everything else here is unchanged.

import SwiftUI

/// How much existing mail the first sync imports.
enum InitialHistory: String, CaseIterable, Identifiable {
    case oneMonth
    case threeMonths
    case twelveMonths
    case all

    var id: String { rawValue }

    var label: String {
        switch self {
        case .oneMonth: return "Last month"
        case .threeMonths: return "Last 3 months"
        case .twelveMonths: return "Last 12 months"
        case .all: return "All mail"
        }
    }

    private var months: Int? {
        switch self {
        case .oneMonth: return 1
        case .threeMonths: return 3
        case .twelveMonths: return 12
        case .all: return nil
        }
    }

    /// The window closest to a stored `initial_sync_since` (`YYYY-MM-DD`), so
    /// the folders sheet opens on the current setting rather than "All mail".
    /// nil/unparseable -> `.all`; older than ~18 months -> `.all`.
    static func nearest(to since: String?, now: Date = Date(),
                        calendar: Calendar = .init(identifier: .gregorian)) -> InitialHistory {
        guard let since else { return .all }
        let f = DateFormatter()
        f.calendar = calendar
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd"
        guard let date = f.date(from: since) else { return .all }
        let months = Double(calendar.dateComponents([.day], from: date, to: now).day ?? 0) / 30.4
        switch months {
        case ..<2: return .oneMonth
        case ..<7: return .threeMonths
        case ..<18: return .twelveMonths
        default: return .all
        }
    }

    /// `YYYY-MM-DD` (the core's `initial_sync_since`) for this window ending
    /// at `now`, or nil for all mail. Gregorian, in `calendar`'s time zone —
    /// IMAP SINCE has day granularity, so the zone only moves the edge by at
    /// most a day.
    func sinceDate(now: Date = Date(), calendar: Calendar = .init(identifier: .gregorian)) -> String? {
        guard let months,
              let start = calendar.date(byAdding: .month, value: -months, to: now) else { return nil }
        let c = calendar.dateComponents([.year, .month, .day], from: start)
        guard let y = c.year, let m = c.month, let d = c.day else { return nil }
        return String(format: "%04d-%02d-%02d", y, m, d)
    }
}

enum SyncScope {
    /// The folders the core would sync with no filter — the picker's initial
    /// ticks.
    static func defaultSelection(_ mailboxes: [ConnectionTestMailbox]) -> Set<String> {
        Set(mailboxes.filter { $0.selectable && $0.syncByDefault }.map(\.path))
    }

    /// The `mailboxes` filter to store for a selection: nil when it equals
    /// the default set (keep the core's live default policy), otherwise the
    /// ticked selectable paths in server order. Non-selectable containers
    /// are never included (they can't be synced).
    static func mailboxFilter(selected: Set<String>, mailboxes: [ConnectionTestMailbox]) -> [String]? {
        let selectable = mailboxes.filter(\.selectable)
        let chosen = Set(selected.filter { path in selectable.contains { $0.path == path } })
        if chosen == defaultSelection(mailboxes) { return nil }
        return selectable.map(\.path).filter { chosen.contains($0) }
    }

    /// Picker rows: selectable folders first in server order, with the
    /// default-synced ones on top so the common case needs no scrolling.
    static func orderedForPicker(_ mailboxes: [ConnectionTestMailbox]) -> [ConnectionTestMailbox] {
        let selectable = mailboxes.filter(\.selectable)
        return selectable.filter(\.syncByDefault) + selectable.filter { !$0.syncByDefault }
    }

    /// Whether Castellum syncs `mailbox` (FP15): with no stored filter, the
    /// core's default policy (`syncByDefault`, true when unknown); with an
    /// explicit filter, whether it contains the mailbox path.
    static func isSynced(mailbox: Mailbox, accountFilter: [String]?) -> Bool {
        guard let accountFilter else { return mailbox.syncByDefault ?? true }
        return accountFilter.contains(mailbox.path)
    }

    // MARK: - Editing an EXISTING account's folders
    //
    // The Settings > Accounts > "Folders…" sheet reuses every function
    // above unchanged (`mailboxFilter` for what to store, `orderedForPicker`
    // for the rows) — these two are the extra pieces that flow only apply.

    /// The picker's initial ticks when editing an existing account: the
    /// account's stored explicit filter if it has one, otherwise the same
    /// live default set a fresh Add would pre-tick. Mirrors `mailboxFilter`'s
    /// own nil-means-defaults contract exactly, so re-opening the sheet
    /// without touching anything round-trips.
    static func currentSelection(stored: [String]?, mailboxes: [ConnectionTestMailbox]) -> Set<String> {
        guard let stored else { return defaultSelection(mailboxes) }
        return Set(stored)
    }

    /// The concrete, resolved set of paths to pass to
    /// `CastellumCore.applyMailboxSelection` (which purges every OTHER
    /// locally known mailbox): `mailboxFilter`'s explicit list when the
    /// selection deviates from the defaults, or the resolved default set
    /// itself when it doesn't — the core purge call needs a real path list
    /// either way, never a nil "use defaults" marker.
    static func resolvedKeepPaths(selected: Set<String>, mailboxes: [ConnectionTestMailbox]) -> [String] {
        if let explicit = mailboxFilter(selected: selected, mailboxes: mailboxes) { return explicit }
        let selectable = mailboxes.filter(\.selectable)
        let defaults = defaultSelection(mailboxes)
        return selectable.map(\.path).filter { defaults.contains($0) }
    }

    /// Applies a Folders-sheet edit to an existing account's stored config:
    /// only `mailboxes` and `initialSyncSince` change. Mutates a COPY of
    /// `config` rather than rebuilding one, so every connection/credential
    /// field carries through untouched by construction — never re-derived,
    /// never re-typed, never at risk of drifting from what Re-authenticate
    /// would have kept.
    static func applyFolderEdit(
        to config: AccountConfig,
        selected: Set<String>,
        mailboxes: [ConnectionTestMailbox],
        initialSyncSince: String?
    ) -> AccountConfig {
        var updated = config
        updated.mailboxes = mailboxFilter(selected: selected, mailboxes: mailboxes)
        updated.initialSyncSince = initialSyncSince
        return updated
    }

    /// The unreachable-server fallback: only the history window can change
    /// (there is no fresh folder list to select against), so the folder
    /// filter is left exactly as stored.
    static func applyHistoryOnlyEdit(to config: AccountConfig, initialSyncSince: String?) -> AccountConfig {
        var updated = config
        updated.initialSyncSince = initialSyncSince
        return updated
    }

    /// Stored explicit paths the live probe found no match for — a folder
    /// renamed or removed server-side since it was last selected (the tracker
    /// #2617 follow-up review, decided default: explain the empty-looking
    /// tick rather than leave a silently disabled Save). `stored` nil (the
    /// defaults marker) never produces a stale path — there is nothing
    /// explicit to go stale.
    static func staleStoredPaths(stored: [String]?, mailboxes: [ConnectionTestMailbox]) -> [String] {
        (stored ?? []).filter { path in
            !mailboxes.contains { $0.path.caseInsensitiveCompare(path) == .orderedSame }
        }
    }
}

/// The folder checklist rows, shared by the S62 first-sync picker
/// (`AccountSetupSheet.firstSyncSection`) and the existing-account editor
/// — same rows (`SyncScope.
/// orderedForPicker`), same checkbox style, same scroll frame. Neither sheet
/// re-implements this.
struct FolderChecklist: View {
    let mailboxes: [ConnectionTestMailbox]
    @Binding var selection: Set<String>

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 2) {
                ForEach(SyncScope.orderedForPicker(mailboxes)) { mailbox in
                    Toggle(mailbox.path, isOn: binding(for: mailbox.path))
                        .toggleStyle(.checkbox)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxHeight: 130)
    }

    private func binding(for path: String) -> Binding<Bool> {
        Binding(
            get: { selection.contains(path) },
            set: { on in
                if on { selection.insert(path) } else { selection.remove(path) }
            }
        )
    }
}

/// The text the message list shows over an empty list (FP15), so a search with
/// no hits, an empty smart mailbox and a folder Castellum does not sync are
/// told apart from "did not run". Pure; nil while there is nothing to explain.
enum MessageListEmptyState {
    static func text(
        messageCount: Int, searchText: String, isSmartMailbox: Bool, hasSelection: Bool,
        isSyncing: Bool, mailboxNotSynced: Bool
    ) -> String? {
        guard messageCount == 0, hasSelection, !isSyncing else { return nil }
        if !searchText.isEmpty { return String(localized: "No messages match “\(searchText)”") }
        if mailboxNotSynced {
            return String(localized: "This folder isn’t synced. Choose it in Settings > Accounts > Folders…")
        }
        if isSmartMailbox { return String(localized: "No messages match this smart mailbox") }
        return String(localized: "No messages")
    }
}
