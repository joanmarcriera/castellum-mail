// Castellum - Split Inbox (HP-2): tab selection and loading for the Inbox.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A split is a group smart mailbox with "Show as a tab in the Inbox" on.
// Rust owns the definitions and the partition (each Inbox message is in the
// first split it matches, or in Other) and returns exact counts; this file
// only owns which tab is selected and what the message list shows.
//
// Entry points:
//   showInboxSplitIfDefined()   from select(mailbox:) / select(unifiedRole:)
//   select(inboxSplit:)         a tab click
//   selectAdjacentInboxSplit()  Next / Previous Split (View menu, ⇧⌘] / ⇧⌘[)
//   reloadInboxSplit()          reloadCurrentMessages() while a tab is open
//   refreshInboxSplitCounts()   after read/unread changes (reloadCounts)
//   inboxSplitDefinitionsChanged(from:)  smartMailboxes didSet
import Foundation

extension AppModel {
    /// Rows per tab: tabs do not page, like a smart mailbox list.
    static let inboxSplitListLimit: Int64 = 500

    /// Whether any split is defined (from the already-loaded smart mailboxes,
    /// so the decision is synchronous at selection time).
    var hasInboxSplits: Bool { smartMailboxes.contains(where: \.isInboxSplit) }

    /// The Inbox the tabs partition: `[id]` for one account's Inbox, `[]` for
    /// All Inboxes, `nil` when the list is not an Inbox at all.
    var inboxSplitMailboxIds: [String]? {
        if let mb = selectedMailbox { return mb.role == "inbox" ? [mb.id] : nil }
        if selectedUnifiedRole == "inbox" { return [] }
        return nil
    }

    /// The list on screen is a split tab (not search, not Correspondence).
    var isInboxSplitActive: Bool {
        hasInboxSplits && !inboxSplitFailed && inboxSplitMailboxIds != nil && selectedSmartMailboxId == nil
            && !selectedIsSnoozed && correspondence == nil && searchText.isEmpty
    }

    /// The tab strip is drawn above the message list.
    var isInboxSplitStripVisible: Bool { isInboxSplitActive && !inboxSplitTabs.isEmpty }

    /// Called right after an Inbox selection reset the list state. Returns
    /// `false` (caller loads the plain paged list) when no split applies.
    func showInboxSplitIfDefined() -> Bool {
        guard isInboxSplitActive else {
            inboxSplitTabs = []
            return false
        }
        loadInboxSplitTabs(thenLoadSelected: true)
        return true
    }

    /// A tab click (or the harness's `selectSplit`).
    func select(inboxSplit id: String) {
        guard isInboxSplitActive else { return }
        selectedInboxSplitId = id
        clearSelection()
        invalidateInFlightPage()
        messages = []
        loadInboxSplitMessages()
    }

    /// Next (`offset` 1) / previous (-1) tab, wrapping round.
    func selectAdjacentInboxSplit(offset: Int) {
        guard isInboxSplitStripVisible else { return }
        let ids = inboxSplitTabs.map(\.id)
        let current = ids.firstIndex(of: selectedInboxSplitId ?? "") ?? 0
        let next = ((current + offset) % ids.count + ids.count) % ids.count
        select(inboxSplit: ids[next])
    }

    /// Refresh the open tab and every count without clearing the selection.
    func reloadInboxSplit() {
        invalidateInFlightPage()
        loadInboxSplitTabs(thenLoadSelected: true)
    }

    /// Counts only (badges), e.g. after a message was marked read.
    func refreshInboxSplitCounts() {
        guard isInboxSplitActive else { return }
        loadInboxSplitTabs(thenLoadSelected: false)
    }

    /// Splits were added, removed, renamed, reordered or re-filtered: rebuild
    /// the tabs if the Inbox is on screen, or fall back to the plain list
    /// when the last split went away.
    func inboxSplitDefinitionsChanged(from old: [SmartMailbox]) {
        let before = old.filter(\.isInboxSplit)
        let after = smartMailboxes.filter(\.isInboxSplit)
        guard before != after, inboxSplitMailboxIds != nil, selectedSmartMailboxId == nil,
              correspondence == nil, searchText.isEmpty else { return }
        if after.isEmpty {
            inboxSplitTabs = []
            if let mb = selectedMailbox { select(mailbox: mb) } else if let role = selectedUnifiedRole { select(unifiedRole: role) }
        } else {
            invalidateInFlightPage()
            loadInboxSplitTabs(thenLoadSelected: true)
        }
    }

    // MARK: loading

    /// Fetch the tab list and counts; keep the selected tab if it still
    /// exists, else open the first one, then (optionally) load its messages.
    private func loadInboxSplitTabs(thenLoadSelected: Bool) {
        guard let ids = inboxSplitMailboxIds else { return }
        let core = self.core
        let token = listToken
        Task.detached {
            let tabs = try? core.inboxSplitCounts(mailboxIds: ids)
            await MainActor.run {
                guard self.isInboxSplitActive, self.inboxSplitMailboxIds == ids else { return }
                guard let tabs else {
                    // Could not partition (core error): show the plain list.
                    self.fallBackFromInboxSplit()
                    return
                }
                self.inboxSplitTabs = tabs
                if !tabs.contains(where: { $0.id == self.selectedInboxSplitId }) {
                    self.selectedInboxSplitId = tabs.first?.id
                }
                if thenLoadSelected, self.listToken == token { self.loadInboxSplitMessages() }
            }
        }
    }

    /// Load the selected tab's rows; a split deleted meanwhile (`nil`)
    /// falls back to Other.
    private func loadInboxSplitMessages() {
        guard let ids = inboxSplitMailboxIds, let splitId = selectedInboxSplitId else { return }
        let core = self.core
        let token = listToken
        Task.detached {
            do {
                let rows = try core.inboxSplitMessages(
                    mailboxIds: ids, splitId: splitId, limit: AppModel.inboxSplitListLimit)
                await MainActor.run {
                    guard self.listToken == token, self.selectedInboxSplitId == splitId,
                          self.isInboxSplitActive else { return }
                    if let rows {
                        self.messages = self.clientSorted(rows)
                    } else if splitId != InboxSplitCount.otherId {
                        self.selectedInboxSplitId = InboxSplitCount.otherId
                        self.loadInboxSplitMessages()
                    }
                }
            } catch {
                Log.ui.error("inbox split load failed: \(error, privacy: .private)")
                await MainActor.run {
                    guard self.listToken == token, self.isInboxSplitActive else { return }
                    self.fallBackFromInboxSplit()
                }
            }
        }
    }

    /// The split query failed: stop treating the Inbox as split and load the plain page,
    /// so the previous mailbox's rows never stay on screen under the Inbox heading (#3441).
    func fallBackFromInboxSplit() {
        inboxSplitFailed = true
        inboxSplitTabs = []
        selectedInboxSplitId = nil
        invalidateInFlightPage()
        loadPlainFirstPage()
    }
}
