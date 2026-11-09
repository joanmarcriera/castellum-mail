// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 ops for the Split Inbox (HP-2). Debug builds only.
//
// Routed from UIReading (one-line hooks), so a scenario uses them as `reading` steps:
//   upsertSplit  name=<name> text=<FilterGroupDTO JSON> [accountRef]   a smart mailbox with
//                "Show as a tab in the Inbox" on (the editor sheet's toggle)
//   selectSplit  text=<tab name | Other>                               a tab click
//   nextSplit / previousSplit                                          View > Next/Previous Split
// Expectation keys (`reading:`):
//   splitStrip            "true" while the tab strip is drawn
//   splitTabs             tab names in order, e.g. "Invoices,Hetzner,Other"
//   splitSelected         the selected tab's name
//   splitOf:<subject>     names of every tab holding a message with that subject ("none" if none)
//   splitsDisjoint        "true" when no message id is in two tabs AND the tabs together hold
//                         exactly the Inbox list (asked of the core, not the screen)
//   splitCountsMatch      "true" when every tab's total/unread equals its own list
#if DEBUG
import Foundation

@MainActor
enum UISplits {
    nonisolated static let actions: Set<String> = ["upsertSplit", "selectSplit", "nextSplit", "previousSplit"]

    nonisolated static func problems(_ s: UIStep) -> [String]? {
        switch s.id ?? "" {
        case "upsertSplit": return s.name != nil && s.text != nil ? [] : ["upsertSplit needs name and text"]
        case "selectSplit": return s.text != nil ? [] : ["selectSplit needs text"]
        case "nextSplit", "previousSplit": return []
        default: return nil
        }
    }

    static func perform(_ s: UIStep, runner r: UIScriptRunner) async throws {
        let model = r.model
        let timeout = s.timeoutMs ?? 15_000
        switch s.id ?? "" {
        case "upsertSplit":
            guard let data = s.text?.data(using: .utf8) else { return }
            let group = try JSONDecoder().decode(FilterGroupDTO.self, from: data) // as UIReading.upsertSmart
            var scope = SmartMailboxScopeDTO()
            scope.inboxSplit = true
            if let a = s.accountRef.flatMap({ r.refs[$0] ?? $0 }) { scope.accountIds = [a] }
            var saved: SmartMailbox?
            var done = false
            model.upsertSmartMailbox(id: nil, name: s.name ?? "", query: "", group: group, scope: scope) {
                saved = $0; done = true
            }
            try await r.poll(timeoutMs: timeout, "split saved") { done }
            if saved == nil { throw UIStepError(message: "split not saved: \(model.smartMailboxSaveError ?? "invalid")") }
        case "selectSplit":
            try await r.poll(timeoutMs: timeout, "split tab '\(s.text ?? "")'") {
                model.inboxSplitTabs.contains { $0.displayName == s.text || $0.name == s.text }
            }
            if let tab = model.inboxSplitTabs.first(where: { $0.displayName == s.text || $0.name == s.text }) {
                model.select(inboxSplit: tab.id)
            }
        case "nextSplit": NotificationCenter.default.post(name: .castellumNextSplit, object: nil)
        case "previousSplit": NotificationCenter.default.post(name: .castellumPreviousSplit, object: nil)
        default: throw UIStepError(message: "unhandled split action")
        }
    }

    /// Every tab with its full row list, straight from the core.
    private static func membership(_ m: AppModel) -> [(tab: InboxSplitCount, rows: [MessageHeader])] {
        guard let ids = m.inboxSplitMailboxIds else { return [] }
        return m.inboxSplitTabs.map { tab in
            let rows = (try? m.core.inboxSplitMessages(mailboxIds: ids, splitId: tab.id, limit: 100_000)) ?? nil
            return (tab, rows ?? [])
        }
    }

    /// The plain Inbox list the tabs partition (every row, newest first).
    private static func inboxRows(_ m: AppModel) -> [MessageHeader] {
        if let mb = m.selectedMailbox {
            return (try? m.core.listMessagesPage(accountId: mb.accountId, mailboxId: mb.id, after: nil, limit: 100_000))?.rows ?? []
        }
        return (try? m.core.listMessagesPage(byRole: "inbox", after: nil, limit: 100_000))?.rows ?? []
    }

    /// Value of a split expectation key; nil = not a split key.
    static func value(_ key: String, model m: AppModel) -> String? {
        switch key {
        case "splitStrip": return String(m.isInboxSplitStripVisible)
        case "splitTabs": return m.inboxSplitTabs.map(\.displayName).joined(separator: ",")
        case "splitSelected":
            return m.inboxSplitTabs.first { $0.id == m.selectedInboxSplitId }?.displayName ?? "none"
        case "splitsDisjoint":
            let all = membership(m).flatMap { $0.rows.map(\.id) }
            let inbox = Set(inboxRows(m).map(\.id))
            return String(!all.isEmpty && all.count == Set(all).count && Set(all) == inbox)
        case "splitCountsMatch":
            return String(membership(m).allSatisfy { tab, rows in
                tab.total == Int64(rows.count) && tab.unread == Int64(rows.filter { !$0.isRead }.count)
            })
        case _ where key.hasPrefix("splitOf:"):
            let subject = String(key.dropFirst(8))
            let tabs = membership(m).filter { $0.rows.contains { ($0.subject ?? "").contains(subject) } }
            return tabs.isEmpty ? "none" : tabs.map(\.tab.displayName).joined(separator: ",")
        default: return nil
        }
    }
}

#endif
