// Castellum - "Move to Folder" (, slice 4): pure planning rules plus
// the popover picker. The move itself is `AppModel.moveSelected(toMailboxId:)`,
// which goes through the same undoable batch call as Archive and Delete, so
// Edit > Undo Move / Cmd-Z / `z` bring the messages back.
//
// Rules (unit-tested in `MoveToFolderPlanTests`, no UI needed):
//   - one account per move: a selection spanning accounts cannot be moved;
//   - only selectable folders of that account are offered;
//   - messages already in the chosen folder are left alone (nothing queued).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

enum MoveToFolderPlan {
    enum Eligibility: Equatable {
        case noSelection
        case multipleAccounts
        case ok(accountId: String)
    }

    /// Which account the selection belongs to, or why it cannot be moved.
    static func eligibility(ids: [String], rows: [MessageHeader]) -> Eligibility {
        guard !ids.isEmpty else { return .noSelection }
        let byId = Dictionary(rows.map { ($0.id, $0.accountId) }, uniquingKeysWith: { first, _ in first })
        let accounts = Set(ids.compactMap { byId[$0] })
        switch accounts.count {
        case 0: return .noSelection
        case 1: return .ok(accountId: accounts.first!)
        default: return .multipleAccounts
        }
    }

    /// The folders offered for `accountId`: selectable ones, special-use folders
    /// first (Inbox, Archive, ... in a stable order), then the rest by name.
    static func destinations(accountId: String, mailboxes: [String: [Mailbox]], query: String = "") -> [Mailbox] {
        let roleOrder = ["inbox", "archive", "sent", "drafts", "trash", "junk"]
        func rank(_ m: Mailbox) -> Int { m.role.flatMap(roleOrder.firstIndex(of:)) ?? roleOrder.count }
        let q = query.trimmingCharacters(in: .whitespaces)
        return (mailboxes[accountId] ?? [])
            .filter { $0.selectable != false }
            .filter { q.isEmpty || $0.name.localizedCaseInsensitiveContains(q) || $0.path.localizedCaseInsensitiveContains(q) }
            .sorted { (rank($0), $0.name.lowercased()) < (rank($1), $1.name.lowercased()) }
    }

    /// The ids that actually need moving: not already in `destinationId`.
    static func idsToMove(ids: [String], rows: [MessageHeader], destinationId: String) -> [String] {
        let byId = Dictionary(rows.map { ($0.id, $0.mailboxId) }, uniquingKeysWith: { first, _ in first })
        return ids.filter { byId[$0] != destinationId }
    }
}

/// The popover: a filter field and the account's folders. Return picks the first match.
struct MoveToFolderPicker: View {
    @ObservedObject var model: AppModel
    var onDismiss: () -> Void = {}

    @State private var query = ""
    @FocusState private var searchFocused: Bool

    private var eligibility: MoveToFolderPlan.Eligibility {
        MoveToFolderPlan.eligibility(ids: model.currentSelectionIds(), rows: model.messages)
    }

    private var folders: [Mailbox] {
        guard case .ok(let accountId) = eligibility else { return [] }
        return MoveToFolderPlan.destinations(
            accountId: accountId, mailboxes: model.mailboxes, query: query)
    }

    var body: some View {
        VStack(spacing: 0) {
            switch eligibility {
            case .noSelection:
                note("Select a message to move.")
            case .multipleAccounts:
                note("Messages from different accounts can’t be moved together. Select messages from one account.")
            case .ok:
                TextField("Move to folder…", text: $query)
                    .textFieldStyle(.roundedBorder)
                    .padding(8)
                    .focused($searchFocused)
                    .onSubmit { if let first = folders.first { choose(first) } }
                Divider()
                ScrollView {
                    LazyVStack(spacing: 0) {
                        if folders.isEmpty {
                            Text("No matching folders")
                                .foregroundStyle(.secondary).font(.caption).padding(.vertical, 12)
                        }
                        ForEach(folders, id: \.id) { folder in
                            Button { choose(folder) } label: {
                                HStack {
                                    Text(folder.name).foregroundStyle(.primary)
                                    Spacer()
                                }
                                .padding(.horizontal, 12).padding(.vertical, 7)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            Divider()
                        }
                    }
                }
                .frame(maxHeight: 240)
            }
        }
        .frame(width: 260)
        .onAppear { searchFocused = true }
    }

    private func note(_ text: LocalizedStringKey) -> some View {
        Text(text).font(.callout).foregroundStyle(.secondary).padding(12)
    }

    private func choose(_ folder: Mailbox) {
        model.moveSelected(toMailboxId: folder.id)
        onDismiss()
    }
}
