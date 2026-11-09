// Castellum - Project detail view (name, notes, linked emails, people).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct ProjectDetailView: View {
    @ObservedObject var model: AppModel
    let projectId: String
    var onDeleted: () -> Void = {}

    @State private var detail: ProjectDetail?
    /// Link-model rows for this project's contacts (evidence popovers).
    @State private var contactLinks: [Int64: LinkRecord] = [:]
    @State private var editingName: String = ""
    @State private var editingNotes: String = ""
    @State private var showLinkPeople = false
    @State private var showDeleteConfirm = false

    private let statusOptions = ["active", "waiting", "someday", "done"]
    private let colorOptions: [(String?, String)] = [
        (nil,        "None"),
        ("#ef4444",  "Red"),
        ("#3b82f6",  "Blue"),
        ("#22c55e",  "Green"),
        ("#f59e0b",  "Amber"),
    ]

    var body: some View {
        Group {
            if let d = detail {
                ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                    // Name + status row
                    HStack {
                        TextField("Project name", text: $editingName)
                            .textFieldStyle(.roundedBorder)
                            .font(.title3.bold())
                            .onChange(of: editingName) { _ in saveProjectFields(d) }

                        Picker("Status", selection: Binding(
                            get: { d.project.status },
                            set: { model.setProjectStatus(d.project.id, status: $0) { reload() } }
                        )) {
                            ForEach(statusOptions, id: \.self) { s in
                                Text(s.capitalized).tag(s)
                            }
                        }
                        .labelsHidden()
                        .frame(width: 120)
                    }

                    // Colour picker row
                    HStack(spacing: 8) {
                        Text("Color:").foregroundStyle(.secondary).font(.caption)
                        ForEach(colorOptions, id: \.0.debugDescription) { hex, name in
                            Button {
                                model.setProjectColor(d.project.id, color: hex) { reload() }
                            } label: {
                                Circle()
                                    .fill(hex.flatMap { Color(hex: $0) } ?? Color(nsColor: .tertiaryLabelColor))
                                    .frame(width: 20, height: 20)
                                    .overlay(
                                        d.project.color == hex
                                            ? Circle().stroke(Color.primary, lineWidth: 2)
                                            : nil
                                    )
                            }
                            .buttonStyle(.plain)
                            .help(name)
                        }
                    }

                    // Notes
                    Text("Notes").font(.headline)
                    TextEditor(text: $editingNotes)
                        .frame(minHeight: 80)
                        .border(Color(nsColor: .separatorColor))
                        .onChange(of: editingNotes) { _ in saveProjectFields(d) }

                    // Linked emails
                    if !d.emails.isEmpty {
                        Text("Linked emails (\(d.emails.count))").font(.headline)
                        ForEach(d.emails) { h in
                            HStack {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(h.subject ?? "(no subject)").font(.body).lineLimit(1)
                                    Text(h.fromAddress ?? "").font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Text(relativeDate(h.sentAt)).font(.caption2).foregroundStyle(.tertiary)
                                Button {
                                    model.unlinkEmail(projectId: d.project.id, messageId: h.id) {
                                        reload()
                                    }
                                } label: {
                                    Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                                }
                                .buttonStyle(.plain)
                            }
                            .contentShape(Rectangle())
                            .onTapGesture { model.openStandaloneMessage(id: h.id) }
                            Divider()
                        }
                    }

                    // People
                    HStack {
                        Text("People (\(d.contacts.count))").font(.headline)
                        Spacer()
                        Button {
                            ContactsWindowController.show(
                                model: model,
                                graphTab: true,
                                projectId: d.project.id
                            )
                        } label: {
                            Label("Show network", systemImage: "point.3.connected.trianglepath.dotted")
                                .font(.caption)
                        }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                        .help("View the co-occurrence network for this project's contacts")
                    }
                    FlowLayout(spacing: 6) {
                        ForEach(d.contacts, id: \.contactId) { link in
                            PersonChipView(
                                link: link,
                                evidence: contactLinks[link.contactId],
                                onOpenMessage: { model.openStandaloneMessage(id: $0) },
                                onConfirm: {
                                    model.confirmAutoContact(projectId: d.project.id,
                                                             contactId: link.contactId) {
                                        reload()
                                    }
                                },
                                onRemove: {
                                    model.unlinkContact(projectId: d.project.id,
                                                        contactId: link.contactId) {
                                        reload()
                                    }
                                }
                            )
                        }
                        Button {
                            showLinkPeople = true
                        } label: {
                            Label("Link people", systemImage: "plus.circle")
                                .font(.caption)
                        }
                        .popover(isPresented: $showLinkPeople) {
                            LinkPeoplePopover(
                                model: model,
                                projectId: d.project.id,
                                linkedContactIds: Set(d.contacts.map(\.contactId))
                            ) {
                                reload()
                            }
                        }
                    }

                    Divider()
                    HStack {
                        Button(d.project.archived ? "Unarchive" : "Archive") {
                            model.setProjectArchived(d.project.id,
                                                     archived: !d.project.archived) { reload() }
                        }
                        .foregroundStyle(d.project.archived ? Color.secondary : Color.orange)

                        Spacer()

                        Button("Delete Project", role: .destructive) {
                            showDeleteConfirm = true
                        }
                        .foregroundStyle(.red)
                        .confirmationDialog(
                            "Delete \"\(d.project.name)\"?",
                            isPresented: $showDeleteConfirm,
                            titleVisibility: .visible
                        ) {
                            Button("Delete", role: .destructive) {
                                model.deleteProject(d.project.id) { deleted in
                                    if deleted { onDeleted() }
                                }
                            }
                        }
                    }
                    }
                    .padding(20)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .navigationTitle(d.project.name)
            } else {
                ProgressView("Loading project…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(minWidth: 700, minHeight: 480)
        .task(id: projectId) { reload() }
    }

    private func reload() {
        model.loadProjectContactLinks(projectId: projectId) { links in
            self.contactLinks = links
        }
        model.loadProjectDetail(id: projectId) { d in
            self.detail = d
            if let d {
                // Only populate the editing fields on first load (empty means not yet set).
                if self.editingName.isEmpty { self.editingName = d.project.name }
                if self.editingNotes.isEmpty { self.editingNotes = d.project.notes }
            }
        }
    }

    private func saveProjectFields(_ d: ProjectDetail) {
        model.saveProjectFields(
            id: d.project.id,
            name: editingName,
            notes: editingNotes,
            status: d.project.status,
            color: d.project.color
        )
    }
}

// MARK: - Person chip

struct PersonChipView: View {
    /// NOTE the dual origin representation: `link.origin` comes from the
    /// ProjectContactLink DTO, where the DB renders stored 'derived' as the
    /// legacy value "auto" (drives the muted styling + Confirm button below).
    /// `evidence?.origin` is the raw LinkRecord and carries "derived" — use
    /// it only for the evidence popover, never for the "auto" comparisons.
    let link: ProjectContactLink
    /// Link-model row backing this chip; drives the "why linked" popover.
    var evidence: LinkRecord?
    var onOpenMessage: (String) -> Void = { _ in }
    let onConfirm: () -> Void
    let onRemove: () -> Void

    @State private var showEvidence = false

    var body: some View {
        HStack(spacing: 4) {
            Button {
                if evidence != nil { showEvidence = true }
            } label: {
                Text(link.displayName ?? link.derivedName ?? link.email)
                    .font(.caption)
                    .foregroundStyle(link.origin == "auto" ? .secondary : .primary)
            }
            .buttonStyle(.plain)
            .help(evidence.map {
                linkEvidenceExplanation(origin: $0.origin, provenance: $0.provenance)
            } ?? "")
            .popover(isPresented: $showEvidence, arrowEdge: .bottom) {
                if let evidence {
                    LinkEvidencePopover(evidence: evidence, onOpenMessage: onOpenMessage)
                }
            }
            if link.origin == "auto" {
                Button { onConfirm() } label: {
                    Image(systemName: "checkmark").font(.caption2)
                }
                .buttonStyle(.plain)
                .foregroundStyle(Color.accentColor)
                .help("Confirm (make manual)")
            }
            Button { onRemove() } label: {
                Image(systemName: "xmark").font(.caption2)
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(Capsule().fill(Color(nsColor: .controlBackgroundColor)))
        .overlay(Capsule().stroke(Color(nsColor: .separatorColor), lineWidth: 0.5))
        .accessibilityElement(children: .contain)
    }
}

/// Compact "why is this connected?" popover for one link-model row.
private struct LinkEvidencePopover: View {
    let evidence: LinkRecord
    let onOpenMessage: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Why linked", systemImage: "link")
                .font(.subheadline).fontWeight(.semibold)
            Text(linkEvidenceExplanation(
                origin: evidence.origin, provenance: evidence.provenance))
                .font(.callout)
                .fixedSize(horizontal: false, vertical: true)
            Text("Since \(linkEvidenceDateText(epoch: evidence.createdAt))")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let note = evidence.note, !note.isEmpty {
                Divider()
                Text(note)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityLabel("Your note: \(note)")
            }
            if let messageId = evidenceMessageId(provenance: evidence.provenance) {
                Divider()
                Button {
                    onOpenMessage(messageId)
                } label: {
                    Label("Open the evidencing message", systemImage: "envelope.open")
                        .font(.caption)
                }
                .buttonStyle(.link)
            } else if let note = linkEvidenceMissingMessageNote(
                origin: evidence.origin, provenance: evidence.provenance) {
                // Say why there is no button. Omitting it silently read as
                // "this link has no reason" rather than "the reason predates
                // evidence tracking".
                Divider()
                // A plain secondary Text, matching the user-note row above —
                // NOT a Label. The only two Labels in this popover are the
                // header and the action, so a Label here would read as a
                // second clickable affordance sitting exactly where the
                // "Open the evidencing message" button would have been.
                Text(note)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(12)
        .frame(width: 280, alignment: .leading)
    }
}

// MARK: - Flow layout

/// Simple flow layout that wraps chips to new rows when the width is exhausted.
struct FlowLayout: Layout {
    var spacing: CGFloat = 4

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        layout(proposal: proposal, subviews: subviews).size
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews,
                       cache: inout ()) {
        let result = layout(proposal: ProposedViewSize(bounds.size), subviews: subviews)
        for (i, subview) in subviews.enumerated() {
            subview.place(
                at: CGPoint(x: bounds.minX + result.origins[i].x,
                            y: bounds.minY + result.origins[i].y),
                proposal: .unspecified
            )
        }
    }

    private func layout(proposal: ProposedViewSize, subviews: Subviews)
        -> (size: CGSize, origins: [CGPoint]) {
        let naturalWidth = subviews.reduce(CGFloat.zero) { partial, subview in
            partial + subview.sizeThatFits(.unspecified).width + spacing
        }
        // Returning an infinite size during the NavigationStack's first layout
        // leaves AppKit with no usable intrinsic content until a manual resize.
        let maxW = proposal.width ?? max(naturalWidth - spacing, 0)
        var origins: [CGPoint] = []
        var x: CGFloat = 0, y: CGFloat = 0, rowH: CGFloat = 0
        for sub in subviews {
            let s = sub.sizeThatFits(.unspecified)
            if x + s.width > maxW && x > 0 { x = 0; y += rowH + spacing; rowH = 0 }
            origins.append(CGPoint(x: x, y: y))
            rowH = max(rowH, s.height)
            x += s.width + spacing
        }
        return (CGSize(width: maxW, height: y + rowH), origins)
    }
}

// MARK: - Link people popover

struct LinkPeoplePopover: View {
    @ObservedObject var model: AppModel
    let projectId: String
    let linkedContactIds: Set<Int64>
    let onLink: () -> Void

    @State private var query = ""
    @Environment(\.dismiss) private var dismiss

    private var filtered: [Contact] {
        LinkPeopleSuggestions.filtered(
            contacts: model.contacts,
            query: query,
            excluding: linkedContactIds
        )
    }

    var body: some View {
        VStack(spacing: 8) {
            TextField("Search contacts…", text: $query)
                .textFieldStyle(.roundedBorder)
                .padding(.horizontal)
            Text(query.isEmpty ? "Recent correspondents" : "Matching contacts")
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal)
            if filtered.isEmpty {
                Text(query.isEmpty
                     ? "All recent correspondents are already linked."
                     : "No matching contacts.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .padding()
            } else {
                List(filtered) { contact in
                    Button {
                        model.linkContact(projectId: projectId, contactId: contact.id, linked: true)
                        onLink()
                        dismiss()
                    } label: {
                        VStack(alignment: .leading) {
                            Text(contact.bestName).font(.body)
                            Text(contact.email).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .buttonStyle(.plain)
                }
                .listStyle(.plain)
            }
        }
        .frame(width: 280, height: 320)
        .onAppear { model.reloadContacts() }
    }
}

enum LinkPeopleSuggestions {
    static func filtered(
        contacts: [Contact],
        query: String,
        excluding linkedContactIds: Set<Int64>,
        limit: Int = 50
    ) -> [Contact] {
        contacts.lazy
            .filter { !linkedContactIds.contains($0.id) }
            .filter {
                query.isEmpty
                    || $0.email.localizedCaseInsensitiveContains(query)
                    || $0.bestName.localizedCaseInsensitiveContains(query)
            }
            .prefix(limit)
            .map { $0 }
    }
}
