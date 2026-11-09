// Castellum - Contacts window content: searchable list + editable detail + graph.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Two tabs:
//   People — filterable contact list + detail pane with read-only derived
//            stats, user-owned fields (display name, notes, Hidden), and an
//            Enrichment section (slice 8d) with per-source field rows, a
//            paste-adapter sheet, and quick-add.
//   Graph  — Fruchterman–Reingold canvas, project-filterable (slice 8c).
//
// The graph tab accepts an optional `filterProjectId` binding so that
// ProjectDetailView's "Show network" button can open the window pre-filtered.

import SwiftUI

struct ContactsView: View {
    @ObservedObject var model: AppModel
    @State private var selectedId: Int64?
    @State private var tab: Tab = .list
    /// When non-nil the Graph tab shows only this project's contacts.
    @Binding var filterProjectId: String?
    /// Full contact detail loaded when a contact is selected (includes enrichment).
    @State private var selectedDetail: ContactDetail?

    private enum Tab: Hashable { case list, graph }

    /// Convenience init for callers that don't need the project filter.
    init(model: AppModel, filterProjectId: Binding<String?> = .constant(nil)) {
        self.model = model
        self._filterProjectId = filterProjectId
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("", selection: $tab) {
                Text("People").tag(Tab.list)
                Text("Graph").tag(Tab.graph)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(8)

            switch tab {
            case .list:
                HSplitView {
                    listPane
                        .frame(minWidth: 320)
                    detailPane
                        .frame(minWidth: 320)
                }
            case .graph:
                ContactGraphView(
                    model: model,
                    projectId: filterProjectId,
                    onSelectContact: { id in
                        if id == -1 {
                            // Clear filter sentinel from the chip's ✕ button.
                            filterProjectId = nil
                        } else {
                            // Switch to the People tab and select the contact.
                            tab = .list
                            selectedId = id
                        }
                    }
                )
            }
        }
        .frame(minWidth: 680, minHeight: 420)
        // When the graph tab is forced open (e.g. from ProjectDetailView) switch
        // to it automatically — both on first open and when the filter is set
        // on an already-open window.
        .onAppear {
            if filterProjectId != nil { tab = .graph }
        }
        .onChange(of: filterProjectId) { newValue in
            if newValue != nil { tab = .graph }
        }
        .onChange(of: selectedId) { newId in
            selectedDetail = nil
            guard let id = newId else { return }
            model.loadContactDetail(id: id) { detail in
                if self.selectedId == id { self.selectedDetail = detail }
            }
        }
    }

    // MARK: - list pane

    private var listPane: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                TextField("Filter by name or email", text: $model.contactsQuery)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit { model.reloadContacts() }
                    .onChange(of: model.contactsQuery) { _ in
                        model.reloadContacts()
                    }
                Toggle("Hidden", isOn: $model.contactsIncludeHidden)
                    .toggleStyle(.checkbox)
                    .help("Also show contacts you have hidden")
                    .onChange(of: model.contactsIncludeHidden) { _ in
                        model.reloadContacts()
                    }
            }
            .padding(8)

            if model.contacts.isEmpty {
                Spacer()
                Text("No contacts yet — sync a mailbox to populate this list.")
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding()
                Spacer()
            } else {
                List(model.contacts, selection: $selectedId) { contact in
                    row(contact).tag(contact.id)
                }
                .listStyle(.inset)
            }
        }
    }

    private func row(_ c: Contact) -> some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(c.bestName).fontWeight(.medium).lineLimit(1)
                    if c.isSelf {
                        Text("me").font(.caption2).padding(.horizontal, 4)
                            .background(Capsule().fill(Color.accentColor.opacity(0.2)))
                    }
                    if c.isHidden {
                        Image(systemName: "eye.slash").font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
                if c.bestName != c.email {
                    Text(c.email).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 2) {
                Text("↓\(c.inboundCount) ↑\(c.outboundCount)")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .help(Text(String(localized: "\(c.inboundCount) received from, \(c.outboundCount) sent to")))
                Text(relativeDate(c.lastSeenAt))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }

    // MARK: - detail pane

    private var detailPane: some View {
        Group {
            if let detail = selectedDetail {
                ContactDetailPane(model: model, detail: detail) {
                    // Reload after saves/deletes.
                    guard let id = selectedId else { return }
                    model.loadContactDetail(id: id) { d in
                        if self.selectedId == id { self.selectedDetail = d }
                    }
                }
                .id(detail.id)
            } else if selectedId != nil {
                VStack {
                    Spacer()
                    ProgressView()
                    Spacer()
                }
                .frame(maxWidth: .infinity)
            } else {
                VStack {
                    Spacer()
                    Text("Select a contact").foregroundStyle(.secondary)
                    Spacer()
                }
                .frame(maxWidth: .infinity)
            }
        }
    }
}

// MARK: - Detail pane

/// Full detail editor: derived stats, user-owned fields, and enrichment section.
private struct ContactDetailPane: View {
    @ObservedObject var model: AppModel
    let detail: ContactDetail
    var onReload: () -> Void

    @State private var displayName: String
    @State private var notes: String
    @State private var isHidden: Bool
    @State private var saved = false
    @State private var showEnrichSheet = false

    init(model: AppModel, detail: ContactDetail, onReload: @escaping () -> Void) {
        self.model = model
        self.detail = detail
        self.onReload = onReload
        _displayName = State(initialValue: detail.displayName ?? "")
        _notes = State(initialValue: detail.notes)
        _isHidden = State(initialValue: detail.isHidden)
    }

    private var isDirty: Bool {
        displayName != (detail.displayName ?? "")
            || notes != detail.notes
            || isHidden != detail.isHidden
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                Text(detail.bestName).font(.title3).bold()
                Text(detail.email).font(.callout).foregroundStyle(.secondary)
                    .textSelection(.enabled)

                GroupBox("Derived from your mail") {
                    Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
                        GridRow {
                            Text("Header name").foregroundStyle(.secondary)
                            Text(detail.derivedName ?? "—")
                        }
                        GridRow {
                            Text("Received / sent").foregroundStyle(.secondary)
                            Text("\(detail.inboundCount) / \(detail.outboundCount)")
                        }
                        let firstSeen = relativeDate(detail.firstSeenAt)
                        let lastSeen = relativeDate(detail.lastSeenAt)
                        GridRow {
                            Text("First seen").foregroundStyle(.secondary)
                            Text(firstSeen.isEmpty ? "—" : firstSeen)
                        }
                        GridRow {
                            Text("Last seen").foregroundStyle(.secondary)
                            Text(lastSeen.isEmpty ? "—" : lastSeen)
                        }
                        if detail.isSelf {
                            GridRow {
                                Text("This is you").foregroundStyle(.secondary)
                                Text("Matches one of your accounts")
                            }
                        }
                    }
                    .font(.callout)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(4)
                }

                GroupBox("Yours (survives refresh)") {
                    VStack(alignment: .leading, spacing: 10) {
                        TextField("Display name (overrides header name)", text: $displayName)
                            .textFieldStyle(.roundedBorder)
                        Text("Notes").font(.caption).foregroundStyle(.secondary)
                        TextEditor(text: $notes)
                            .font(.body)
                            .frame(minHeight: 90)
                            .overlay(RoundedRectangle(cornerRadius: 4)
                                .stroke(Color.secondary.opacity(0.3)))
                        Toggle("Hidden (kept out of the default list)", isOn: $isHidden)
                    }
                    .padding(4)
                }

                HStack {
                    if saved {
                        Label("Saved", systemImage: "checkmark.circle")
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Save") { save() }
                        .keyboardShortcut("s", modifiers: .command)
                        .disabled(!isDirty)
                }

                // ── Enrichment section (slice 8d) ───────────────────────────
                enrichmentSection
            }
            .padding(16)
        }
        .sheet(isPresented: $showEnrichSheet) {
            EnrichSheet(model: model, contactId: detail.id, onSaved: {
                showEnrichSheet = false
                onReload()
            })
        }
    }

    private func save() {
        let trimmed = displayName.trimmingCharacters(in: .whitespaces)
        model.updateContact(
            id: detail.id,
            displayName: trimmed.isEmpty ? nil : trimmed,
            notes: notes,
            isHidden: isHidden
        ) { ok in
            saved = ok
        }
    }

    // MARK: Enrichment section

    @ViewBuilder
    private var enrichmentSection: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 10) {
                // Group fields by source.
                let grouped = Dictionary(grouping: detail.enrichment, by: \.source)
                let sortedSources = grouped.keys.sorted()

                if detail.enrichment.isEmpty {
                    Text("No enrichment data yet.")
                        .foregroundStyle(.secondary)
                        .font(.callout)
                        .padding(.vertical, 4)
                } else {
                    ForEach(sortedSources, id: \.self) { source in
                        let rows = grouped[source] ?? []
                        VStack(alignment: .leading, spacing: 4) {
                            Text(source.capitalized)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            ForEach(rows) { row in
                                HStack(alignment: .top) {
                                    Text(row.field)
                                        .font(.callout)
                                        .foregroundStyle(.secondary)
                                        .frame(minWidth: 80, alignment: .leading)
                                    Text(row.value)
                                        .font(.callout)
                                        .help(row.provenance ?? "")
                                        .textSelection(.enabled)
                                    Spacer()
                                    Button {
                                        model.deleteEnrichment(id: row.id) {
                                            onReload()
                                        }
                                    } label: {
                                        Image(systemName: "xmark.circle")
                                            .foregroundStyle(.secondary)
                                    }
                                    .buttonStyle(.plain)
                                    .help("Remove this field")
                                }
                            }
                        }
                        Divider()
                    }
                }

                // Quick key-value add (writes to 'manual' source directly).
                QuickAddRow(contactId: detail.id, model: model, onAdded: onReload)

                HStack {
                    Spacer()
                    Button("Add info…") { showEnrichSheet = true }
                }
            }
            .padding(4)
        } label: {
            Text("Enrichment")
        }
    }
}

// MARK: - Quick key-value add

/// Two text fields + "Add" button for writing a free-form field directly
/// to the 'manual' source without going through the adapter sheet.
private struct QuickAddRow: View {
    let contactId: Int64
    @ObservedObject var model: AppModel
    var onAdded: () -> Void

    @State private var fieldName = ""
    @State private var fieldValue = ""

    var body: some View {
        HStack(spacing: 6) {
            TextField("Field", text: $fieldName)
                .textFieldStyle(.roundedBorder)
                .frame(maxWidth: 100)
            TextField("Value", text: $fieldValue)
                .textFieldStyle(.roundedBorder)
            Button("Add") { add() }
                .disabled(fieldName.trimmingCharacters(in: .whitespaces).isEmpty
                          || fieldValue.trimmingCharacters(in: .whitespaces).isEmpty)
        }
        .font(.callout)
    }

    private func add() {
        let f = fieldName.trimmingCharacters(in: .whitespaces)
        let v = fieldValue.trimmingCharacters(in: .whitespaces)
        guard !f.isEmpty, !v.isEmpty else { return }
        model.addEnrichmentField(contactId: contactId, field: f, value: v) {
            fieldName = ""
            fieldValue = ""
            onAdded()
        }
    }
}

// MARK: - Enrich sheet

/// Sheet: pick an adapter source, paste text, preview, save.
/// All core calls go through AppModel (which owns the private core handle).
private struct EnrichSheet: View {
    @ObservedObject var model: AppModel
    let contactId: Int64
    var onSaved: () -> Void

    @State private var sources: [EnrichmentSourceInfo] = []
    @State private var selectedSourceId: String = "manual"
    @State private var inputText = ""
    @State private var previewFields: [PreviewEnrichmentField] = []
    @State private var isPreviewing = false
    @State private var isSaving = false
    @State private var errorMsg: String?

    var selectedSource: EnrichmentSourceInfo? {
        sources.first(where: { $0.id == selectedSourceId })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add Enrichment Info")
                .font(.title3).bold()

            if sources.count > 1 {
                Picker("Source", selection: $selectedSourceId) {
                    ForEach(sources) { src in
                        Text(src.displayName).tag(src.id)
                    }
                }
                .pickerStyle(.menu)
            }

            VStack(alignment: .leading, spacing: 4) {
                Text(selectedSource?.inputLabel ?? "Paste profile text or URL")
                    .font(.caption).foregroundStyle(.secondary)
                TextEditor(text: $inputText)
                    .font(.body)
                    .frame(minHeight: 120)
                    .overlay(RoundedRectangle(cornerRadius: 6)
                        .stroke(Color.secondary.opacity(0.3)))
            }

            if !previewFields.isEmpty {
                GroupBox("Preview") {
                    Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                        ForEach(previewFields, id: \.self) { f in
                            GridRow {
                                Text(f.field).foregroundStyle(.secondary)
                                Text(f.value).textSelection(.enabled)
                            }
                        }
                    }
                    .font(.callout)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(4)
                }
            }

            if let err = errorMsg {
                Text(err).foregroundStyle(.red).font(.callout)
            }

            HStack {
                Button("Cancel") { onSaved() /* dismiss */ }
                Spacer()
                Button("Preview") { preview() }
                    .disabled(inputText.trimmingCharacters(in: .whitespaces).isEmpty || isPreviewing)
                Button("Save") { save() }
                    .disabled(previewFields.isEmpty || isSaving)
                    .buttonStyle(.borderedProminent)
            }
        }
        .padding(20)
        .frame(minWidth: 440, minHeight: 300)
        .onAppear { loadSources() }
    }

    private func loadSources() {
        model.loadEnrichmentSources { loaded in
            // Fall back to a static "manual" entry if the core call failed
            // (the error itself is surfaced via model.errorText).
            sources = loaded.isEmpty ? [
                EnrichmentSourceInfo(id: "manual", displayName: "Manual paste",
                                     kind: "PasteImport",
                                     inputLabel: "Paste profile text or URL")
            ] : loaded
            if !sources.contains(where: { $0.id == selectedSourceId }),
               let first = sources.first {
                selectedSourceId = first.id
            }
        }
    }

    private func preview() {
        isPreviewing = true
        errorMsg = nil
        previewFields = []
        model.previewEnrichment(
            contactId: contactId, sourceId: selectedSourceId, input: inputText
        ) { result in
            isPreviewing = false
            switch result {
            case .success(let fields):
                previewFields = fields
            case .failure(let error):
                errorMsg = String(describing: error)
            }
        }
    }

    private func save() {
        isSaving = true
        errorMsg = nil
        model.saveEnrichment(
            contactId: contactId, sourceId: selectedSourceId, input: inputText
        ) { result in
            isSaving = false
            switch result {
            case .success:
                onSaved()
            case .failure(let error):
                errorMsg = String(describing: error)
            }
        }
    }
}
