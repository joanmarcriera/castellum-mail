// Castellum - Snippets window content.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Structure copied from `SignatureEditorView`: a list with + and - on the left;
// name, abbreviation, account, text and a live preview with problems on the right.
// Edits are kept in a local draft and stored through `TemplateLibrary.update`, so an
// invalid value (a duplicate abbreviation, an over-long name) is shown and refused
// instead of being saved half-way.

import SwiftUI

struct SnippetEditorView: View {
    @ObservedObject var prefs: Preferences
    @ObservedObject var model: AppModel
    @State private var selection: String?
    @State private var draft: SnippetItem?
    @State private var problems: [TemplateProblem] = []
    @State private var pendingDelete: SnippetItem?
    @State private var showHelp = false

    init(prefs: Preferences, model: AppModel) {
        self.prefs = prefs
        self.model = model
        let first = prefs.templateLibrary.snippets.first
        _selection = State(initialValue: first?.id)
        _draft = State(initialValue: first)
    }

    private var library: TemplateLibrary { prefs.templateLibrary }

    var body: some View {
        VStack(spacing: 0) {
            HSplitView {
                list.frame(minWidth: 190, idealWidth: 220, maxWidth: 320)
                detail.frame(minWidth: 380, maxWidth: .infinity)
            }
            Divider()
            ForwardHeaderEditor(model: model, prefs: prefs)
            Divider()
            HStack {
                Toggle("Expand ;abbreviation when I press Tab", isOn: $prefs.templateLibrary.inlineTriggerEnabled)
                    .help(String(localized: "In the message body, type ; and an abbreviation, then press Tab. Off by default: Tab only expands when an abbreviation matches."))
                Spacer()
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .frame(minWidth: SnippetEditorWindowController.minimumContentSize.width,
               minHeight: SnippetEditorWindowController.minimumContentSize.height)
        .onChange(of: selection) { id in load(id) }
        .confirmationDialog(
            String(localized: "Delete “\(pendingDelete?.name ?? "")”?"),
            isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }),
            presenting: pendingDelete
        ) { item in
            Button("Delete", role: .destructive) { delete(item.id) }
            Button("Cancel", role: .cancel) { pendingDelete = nil }
        } message: { _ in
            Text("The snippet text cannot be recovered.")
        }
    }

    // MARK: list

    private var list: some View {
        VStack(spacing: 0) {
            List(selection: $selection) {
                ForEach(library.snippets.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }) { item in
                    VStack(alignment: .leading, spacing: 1) {
                        Text(item.name.isEmpty ? String(localized: "Untitled") : item.name).lineLimit(1)
                        Text(scopeLabel(item)).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    .tag(item.id)
                }
            }
            .listStyle(.inset)
            .overlay {
                if library.snippets.isEmpty {
                    Text("No snippets yet").font(.callout).foregroundStyle(.secondary)
                }
            }
            Divider()
            HStack(spacing: 4) {
                Button { add() } label: { Image(systemName: "plus") }
                    .buttonStyle(.borderless)
                    .help(String(localized: "Add a snippet"))
                    .accessibilityLabel(String(localized: "Add a snippet"))
                Button { pendingDelete = selection.flatMap { library.snippet(id: $0) } } label: { Image(systemName: "minus") }
                    .buttonStyle(.borderless)
                    .help(String(localized: "Delete the selected snippet"))
                    .accessibilityLabel(String(localized: "Delete the selected snippet"))
                    .disabled(selection == nil)
                Spacer()
            }
            .padding(6)
        }
    }

    private func scopeLabel(_ item: SnippetItem) -> String {
        let scope = item.accountId.flatMap { id in model.accounts.first { $0.id == id }?.email }
            ?? String(localized: "All accounts")
        if let abbreviation = item.abbreviation, !abbreviation.isEmpty { return ";\(abbreviation) · \(scope)" }
        return scope
    }

    // MARK: detail

    @ViewBuilder
    private var detail: some View {
        if let draft {
            VStack(alignment: .leading, spacing: 10) {
                TextField("Name", text: field(\.name))
                HStack {
                    TextField("Abbreviation", text: Binding(
                        get: { draft.abbreviation ?? "" },
                        set: { value in edit { $0.abbreviation = value.lowercased() } }))
                        .frame(maxWidth: 190)
                        .help(String(localized: "Optional. Used for typing ;abbreviation and pressing Tab when that is switched on."))
                    Picker("Account", selection: Binding(get: { draft.accountId }, set: { value in edit { $0.accountId = value } })) {
                        Text("All accounts").tag(String?.none)
                        ForEach(model.accounts) { Text($0.email).tag(String?.some($0.id)) }
                    }
                    .fixedSize()
                    Spacer()
                    Button { showHelp.toggle() } label: { Image(systemName: "questionmark.circle") }
                        .buttonStyle(.plain)
                        .help(String(localized: "Placeholders you can use"))
                        .accessibilityLabel(String(localized: "Placeholders you can use"))
                        .popover(isPresented: $showHelp) { TemplatePlaceholderHelp() }
                }
                TextField("Subject (fills an empty Subject field)", text: Binding(
                    get: { draft.subject ?? "" }, set: { value in edit { $0.subject = value } }))
                    .help(String(localized: "Optional. Used only when the message's Subject is still empty. One line."))
                TextEditor(text: field(\.text))
                    .font(.system(.body))
                    .frame(minHeight: 110)
                    .overlay(RoundedRectangle(cornerRadius: 4).stroke(Color.secondary.opacity(0.25)))
                Text("Preview").font(.caption).foregroundStyle(.secondary)
                Text(preview(draft)).font(.callout)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(6)
                    .background(Color.primary.opacity(0.03))
                    .textSelection(.enabled)
                ForEach(Array(allProblems(draft).enumerated()), id: \.offset) { _, problem in
                    Text(problem.source.isEmpty ? problem.message : "\(problem.source): \(problem.message)")
                        .font(.caption).foregroundStyle(.red)
                }
            }
            .textFieldStyle(.roundedBorder)
            .padding(16)
        } else {
            VStack(spacing: 8) {
                Text("No snippet selected").font(.headline)
                Text("Use + to add a snippet.").font(.caption).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func field(_ keyPath: WritableKeyPath<SnippetItem, String>) -> Binding<String> {
        Binding(get: { draft?[keyPath: keyPath] ?? "" }, set: { value in edit { $0[keyPath: keyPath] = value } })
    }

    /// Rendered against a fixed sample so the preview never depends on real mail.
    private func preview(_ item: SnippetItem) -> String {
        let account = model.accounts.first { $0.id == item.accountId } ?? model.accounts.first
        let context = ReplyAttributionEditor.sampleContext(
            for: account ?? Account(id: "", email: "me@example.org", displayName: nil, provider: "imap"))
        let text = TemplateFormatter.render(item.text, context: context, mode: .multiLine).text
        return text.isEmpty ? String(localized: "(empty)") : text
    }

    private func allProblems(_ item: SnippetItem) -> [TemplateProblem] {
        problems + TemplateFormatter.problems(in: item.text, mode: .multiLine)
    }

    // MARK: actions

    private func load(_ id: String?) {
        problems = []
        draft = id.flatMap { library.snippet(id: $0) }
    }

    /// Apply an edit to the draft and try to store it; a refused edit stays in the draft with its problems.
    private func edit(_ change: (inout SnippetItem) -> Void) {
        guard var item = draft else { return }
        change(&item)
        draft = item
        problems = prefs.templateLibrary.update(item)
    }

    private func add() {
        let name = library.suggestedName(accountId: nil, base: String(localized: "Snippet"))
        let item = prefs.templateLibrary.add(name: name, accountId: nil)
        selection = item.id
        draft = item
        problems = []
    }

    private func delete(_ id: String) {
        prefs.templateLibrary.delete(id: id)
        pendingDelete = nil
        selection = library.snippets.first?.id
        load(selection)
    }
}
