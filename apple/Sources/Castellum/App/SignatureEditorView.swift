// Castellum - Signatures window content (GUI-4, redesigned for;
// HTML editing #2286). Design: project-docs/signature-picker-design.md.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Why a window: the inline editor under Settings > Accounts covered the
// account list. See DECISIONS.md 2026-09-30 "Sheet vs window".
// Layout: an Account pop-up on top chooses whose signatures are listed (one
// account at a time, so two similar accounts can't be confused); the list has
// +, - and Set as Default under it; the editor has Name and a Plain Text |
// HTML switch. The compose footer picker chooses among these per message.

import SwiftUI

struct SignatureEditorView: View {
    enum Format: Hashable { case plain, html }

    @ObservedObject var prefs: Preferences
    @ObservedObject var model: AppModel
    @State private var accountId: String?
    @State private var selection: String?
    @State private var format: Format = .plain
    @State private var pendingDelete: SignatureItem?
    @State private var previewHtml = ""
    @ObservedObject private var request = SignatureEditorWindowController.request

    init(prefs: Preferences, model: AppModel, initialAccountId: String? = nil) {
        self.prefs = prefs
        self.model = model
        let library = prefs.signatureLibrary
        let account = initialAccountId
            ?? model.accounts.first { !library.items(for: $0.id).isEmpty }?.id
            ?? model.accounts.first?.id
        _accountId = State(initialValue: account)
        let selected = account.flatMap { Self.preferredSelection(in: library, account: $0) }
        _selection = State(initialValue: selected)
        _format = State(initialValue: Self.preferredFormat(selected.flatMap(library.item(id:))))
    }

    /// The tab that has the content: HTML for an HTML-only signature, else Plain Text.
    private static func preferredFormat(_ item: SignatureItem?) -> Format {
        guard let item, item.htmlIfAny != nil,
              item.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return .plain }
        return .html
    }

    private var library: SignatureLibrary { prefs.signatureLibrary }
    private var accounts: [Account] { model.accounts }
    private var items: [SignatureItem] { accountId.map { library.items(for: $0) } ?? [] }

    /// The account's default, else its first signature, else nothing.
    private static func preferredSelection(in library: SignatureLibrary, account: String) -> String? {
        library.defaultItem(for: account)?.id ?? library.items(for: account).first?.id
    }

    var body: some View {
        VStack(spacing: 0) {
            accountBar
            Divider()
            HSplitView {
                list.frame(minWidth: 190, idealWidth: 220, maxWidth: 320)
                detail.frame(minWidth: 360, maxWidth: .infinity)
            }
        }
        .frame(minWidth: SignatureEditorWindowController.minimumContentSize.width,
               minHeight: SignatureEditorWindowController.minimumContentSize.height)
        .onChange(of: request.token) { _ in
            // "Signatures…" pressed for another account while open: jump to it.
            if let id = request.accountId { show(account: id) }
        }
        .onChange(of: selection) { id in format = Self.preferredFormat(id.flatMap(library.item(id:))) }
        .onChange(of: accounts.map(\.id)) { ids in
            if let accountId, !ids.contains(accountId) { show(account: ids.first) }
        }
        .confirmationDialog(
            deleteTitle,
            isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }),
            presenting: pendingDelete
        ) { item in
            Button("Delete", role: .destructive) { delete(item.id) }
            Button("Cancel", role: .cancel) { pendingDelete = nil }
        } message: { _ in
            Text("The signature text cannot be recovered.")
        }
    }

    private var deleteTitle: String {
        let name = pendingDelete?.name ?? ""
        return String(localized: "Delete “\(name)”?")
    }

    // MARK: account bar

    private var accountBar: some View {
        HStack {
            Picker("Account", selection: Binding(get: { accountId }, set: { show(account: $0) })) {
                ForEach(accounts) { Text($0.email).tag(String?.some($0.id)) }
            }
            .fixedSize()
            .disabled(accounts.isEmpty)
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    // MARK: list

    private var list: some View {
        VStack(spacing: 0) {
            List(selection: $selection) {
                ForEach(items) { item in
                    HStack {
                        Text(item.name.isEmpty ? String(localized: "Untitled") : item.name)
                            .lineLimit(1)
                        Spacer()
                        if library.isDefault(item.id) {
                            Text("Default").font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .tag(item.id)
                }
            }
            .listStyle(.inset)
            .overlay {
                if items.isEmpty {
                    Text("No signatures yet")
                        .font(.callout).foregroundStyle(.secondary)
                }
            }
            Divider()
            HStack(spacing: 4) {
                Button {
                    addSignature()
                } label: {
                    Image(systemName: "plus")
                }
                .buttonStyle(.borderless)
                .help(String(localized: "Add a signature to this account"))
                .accessibilityLabel(String(localized: "Add a signature to this account"))
                .disabled(accountId == nil)
                Button {
                    pendingDelete = selection.flatMap { library.item(id: $0) }
                } label: {
                    Image(systemName: "minus")
                }
                .buttonStyle(.borderless)
                .help(String(localized: "Delete the selected signature"))
                .accessibilityLabel(String(localized: "Delete the selected signature"))
                .disabled(selection == nil)
                Spacer()
                Button("Set as Default") {
                    if let selection { prefs.signatureLibrary.setDefault(id: selection) }
                }
                .controlSize(.small)
                .help(String(localized: "New messages from this account start with this signature"))
                .disabled(selection.map { library.isDefault($0) } ?? true)
            }
            .padding(6)
        }
    }

    // MARK: detail

    @ViewBuilder
    private var detail: some View {
        if let id = selection, let item = library.item(id: id), item.accountId == accountId {
            VStack(alignment: .leading, spacing: 10) {
                TextField("Name", text: Binding(
                    get: { library.item(id: id)?.name ?? "" },
                    set: { prefs.signatureLibrary.rename(id: id, to: $0) }))
                Picker("Format", selection: $format) {
                    Text("Plain Text").tag(Format.plain)
                    Text("HTML").tag(Format.html)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                switch format {
                case .plain: plainEditor(id: id, item: item)
                case .html: htmlEditor(id: id)
                }
            }
            .textFieldStyle(.roundedBorder)
            .padding(16)
        } else {
            VStack(spacing: 8) {
                Text("No signature selected").font(.headline)
                Text("Use + to add a signature to this account.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder
    private func plainEditor(id: String, item: SignatureItem) -> some View {
        sourceEditor(Binding(
            get: { library.item(id: id)?.text ?? "" },
            set: { prefs.signatureLibrary.setText(id: id, $0) }), monospaced: false)
            .frame(minHeight: 100)
        if item.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, item.htmlIfAny != nil {
            Text("Empty: messages use the text of the HTML version.")
                .font(.caption).foregroundStyle(.secondary)
        }
        Text("Plain text. Inserted below “-- ” in new messages and replies.")
            .font(.caption).foregroundStyle(.secondary)
    }

    @ViewBuilder
    private func htmlEditor(id: String) -> some View {
        let html = library.item(id: id)?.html ?? ""
        sourceEditor(Binding(
            get: { library.item(id: id)?.html ?? "" },
            set: { prefs.signatureLibrary.setHTML(id: id, $0) }), monospaced: true)
            .frame(minHeight: 70)
        Text("Preview").font(.caption).foregroundStyle(.secondary)
        EmailBodyWebView(sanitizedHtml: previewHtml, allowRemoteImages: false, onUnrecoverableFailure: {})
            .frame(minHeight: 60)
            .background(Color.primary.opacity(0.03))
            .overlay(RoundedRectangle(cornerRadius: 4).stroke(Color.secondary.opacity(0.25)))
            .task(id: html) {
                // Debounced: the same render (and sanitiser) the HTML part gets.
                try? await Task.sleep(nanoseconds: 250_000_000)
                guard !Task.isCancelled else { return }
                previewHtml = html.isEmpty ? "" : model.core.renderMarkdownPreview(SignatureHTML.previewSource(html))
            }
        Text("Used for the HTML part of messages sent with Markdown on; plain-text messages use the Plain Text version. Allowed: links, bold, italic, lists, headings, tables, line breaks. Images, styles and scripts are removed.")
            .font(.caption).foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
    }

    private func sourceEditor(_ text: Binding<String>, monospaced: Bool) -> some View {
        TextEditor(text: text)
            .font(.system(.body, design: monospaced ? .monospaced : .default))
            .overlay(RoundedRectangle(cornerRadius: 4).stroke(Color.secondary.opacity(0.25)))
    }

    // MARK: actions

    private func show(account id: String?) {
        accountId = id
        selection = id.flatMap { Self.preferredSelection(in: library, account: $0) }
    }

    private func addSignature() {
        guard let accountId else { return }
        let name = library.suggestedName(for: accountId, base: String(localized: "Signature"))
        selection = prefs.signatureLibrary.add(name: name, accountId: accountId)
    }

    private func delete(_ id: String) {
        prefs.signatureLibrary.delete(id: id)
        pendingDelete = nil
        selection = accountId.flatMap { Self.preferredSelection(in: library, account: $0) }
    }
}
