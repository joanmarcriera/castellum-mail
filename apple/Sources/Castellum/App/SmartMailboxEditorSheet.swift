// Castellum - create/rename a smart mailbox (saved S39 search) (S41).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

/// Presented from the toolbar's "Save Search…" (creating — `existing == nil`;
/// opens on the search-text tab prefilled with the current search field, or on
/// the Conditions tab when the field is empty) and from a sidebar smart
/// mailbox's "Edit…" context menu item (`existing != nil`). MM-1: a smart
/// mailbox is either a saved search-text query or a nested all/any/none
/// Conditions group with an optional account/mailbox scope.
struct SmartMailboxEditorSheet: View {
    enum Mode: Hashable { case query, conditions }

    @ObservedObject var model: AppModel
    let existing: SmartMailbox?
    var prefilledQuery: String = ""
    @Environment(\.dismiss) private var dismiss

    @State private var name: String
    @State private var query: String
    @State private var mode: Mode
    @State private var group: FilterGroupDTO
    @State private var scope: SmartMailboxScopeDTO
    @State private var isSaving = false

    init(model: AppModel, existing: SmartMailbox?, prefilledQuery: String = "") {
        self.model = model
        self.existing = existing
        self.prefilledQuery = prefilledQuery
        _name = State(initialValue: existing?.name ?? "")
        _query = State(initialValue: existing?.query ?? prefilledQuery)
        // A stored mailbox with no text query but no readable group (written
        // by a newer build) opens on Conditions too, so editing it can never
        // overwrite the unreadable group with an empty text query.
        let startsInConditions = existing?.group != nil
            || (existing == nil && prefilledQuery.isEmpty)
            || (existing != nil && existing?.query.isEmpty == true)
        _mode = State(initialValue: startsInConditions ? .conditions : .query)
        _group = State(initialValue: existing?.group ?? .starter())
        _scope = State(initialValue: existing?.scope ?? .everywhere)
    }

    private var canSave: Bool {
        guard !name.trimmingCharacters(in: .whitespaces).isEmpty, !isSaving else { return false }
        switch mode {
        case .query: return !query.trimmingCharacters(in: .whitespaces).isEmpty
        case .conditions: return group.isValidForSave(policy: model.headerPolicy)
        }
    }

    private var accountOptions: [(id: String, label: String)] {
        model.accounts.map { (id: $0.id, label: $0.email) }
    }

    private var mailboxOptions: [(id: String, label: String)] {
        model.accounts.flatMap { account in
            (model.mailboxes[account.id] ?? []).map {
                (id: $0.id, label: model.accounts.count > 1 ? "\(account.email) — \($0.name)" : $0.name)
            }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(existing == nil ? "Save Search" : "Edit Smart Mailbox")
                .font(.headline)
            TextField("Name", text: $name)
                .textFieldStyle(.roundedBorder)
            Picker("", selection: $mode) {
                Text("Search").tag(Mode.query)
                Text("Conditions").tag(Mode.conditions)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            switch mode {
            case .query: queryTab
            case .conditions: conditionsTab
            }
            if let error = model.smartMailboxSaveError {
                Text(error).font(.caption).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button(existing == nil ? "Save" : "Save Changes") { save() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canSave)
            }
        }
        .padding(20)
        .frame(width: mode == .conditions ? 600 : 420)
        .onAppear { model.refreshHeaderPolicy() }
        .onDisappear { model.smartMailboxSaveError = nil }
    }

    @ViewBuilder
    private var queryTab: some View {
        TextField("Query", text: $query)
            .textFieldStyle(.roundedBorder)
            .font(.system(.body, design: .monospaced))
        Text(String(localized: "Searches every account, using the same syntax as the search field (from: to: subject: has:attachment before: after: in: is:, \"exact phrase\", -exclude)."))
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    @ViewBuilder
    private var conditionsTab: some View {
        ScrollView {
            FilterGroupEditor(group: $group, headerPolicy: model.headerPolicy)
                .padding(.trailing, 4)
        }
        .frame(minHeight: 140, maxHeight: 320)
        HStack(spacing: 8) {
            Text("Look in:")
            ScopeMenu(title: String(localized: "Accounts"), options: accountOptions, selection: $scope.accountIds)
            ScopeMenu(title: String(localized: "Mailboxes"), options: mailboxOptions, selection: $scope.mailboxIds)
            Spacer()
        }
        // HP-2: the same conditions can also split the Inbox into a tab.
        // MM-1c slice 1: not with a Header condition (the core refuses it).
        Toggle("Show as a tab in the Inbox", isOn: Binding(
            get: { scope.inboxSplit && !group.containsHeaderCondition },
            set: { scope.inboxSplit = $0 }
        ))
            .disabled(group.containsHeaderCondition)
            .help(Text("Inbox mail matching these conditions moves to its own tab. A message matching several tabs goes to the first one; the rest stays in Other."))
        if group.containsHeaderCondition {
            Text(String(localized: "Header conditions cannot split the Inbox yet."))
                .font(.caption)
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("headerInboxSplitReason")
        }
        if group.containsHeaderCondition, let coverage = model.headerCoverage {
            // MM-1c: header conditions only see messages whose headers are stored.
            Text(String(localized: "Headers known: \(coverage.captured) of \(coverage.total)"))
                .font(.caption)
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("headerCoverageCaption")
        }
        Text(String(localized: "Attachment conditions only see messages whose attachment list has been fetched. Relative dates (within the last N days) stay current as time passes."))
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    private func save() {
        isSaving = true
        model.upsertSmartMailbox(
            id: existing?.id, name: name, query: query,
            group: mode == .conditions ? group : nil,
            scope: mode == .conditions ? scope.forSave(group: group) : nil
        ) { saved in
            isSaving = false
            if saved != nil { dismiss() }
        }
    }
}
