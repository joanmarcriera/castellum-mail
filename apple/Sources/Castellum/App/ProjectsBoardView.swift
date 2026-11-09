// Castellum - Projects Kanban board window.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI
import AppKit

struct ProjectsBoardView: View {
    @ObservedObject var model: AppModel
    @State private var showArchived = false
    @State private var showNewProject = false
    @State private var navigationPath = NavigationPath()

    private let columns: [(String, String)] = [
        ("active", "Active"),
        ("waiting", "Waiting"),
        ("someday", "Someday"),
        ("done", "Done"),
    ]

    var body: some View {
        NavigationStack(path: $navigationPath) {
            VStack(spacing: 0) {
                // Toolbar row
                HStack {
                    Button("New Project") { showNewProject = true }
                    Spacer()
                    Toggle("Archived", isOn: $showArchived)
                        .toggleStyle(.checkbox)
                        .onChange(of: showArchived) { _ in
                            model.reloadProjects(includeArchived: showArchived)
                        }
                }
                .padding(10)
                Divider()
                // Board
                let displayed = model.projects.filter { showArchived || !$0.archived }
                HStack(alignment: .top, spacing: 1) {
                    ForEach(columns, id: \.0) { status, label in
                        KanbanColumnView(
                            model: model,
                            status: status,
                            label: label,
                            cards: displayed.filter { $0.status == status }
                        ) { cardId in
                            navigationPath.append(cardId)
                        }
                    }
                }
            }
            .frame(minWidth: 800, minHeight: 500)
            .navigationDestination(for: String.self) { projectId in
                ProjectDetailView(model: model, projectId: projectId) {
                    if !navigationPath.isEmpty { navigationPath.removeLast() }
                }
            }
        }
        .sheet(isPresented: $showNewProject) {
            NewProjectSheet(model: model) { projectId in
                navigationPath.append(projectId)
            }
        }
        .onAppear { model.reloadProjects(includeArchived: showArchived) }
    }
}

// MARK: - New project flow

struct NewProjectDraft: Equatable {
    var name = ""
    var notes = ""
    var status = "active"
    var color: String?

    var trimmedName: String {
        name.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    var canCreate: Bool { !trimmedName.isEmpty }
}

struct NewProjectSheet: View {
    @ObservedObject var model: AppModel
    let onCreated: (String) -> Void

    @State private var draft = NewProjectDraft()
    @State private var isCreating = false
    @FocusState private var focusedField: Field?
    @Environment(\.dismiss) private var dismiss

    private enum Field: Hashable { case name, notes }

    private let statusOptions = ["active", "waiting", "someday"]
    private let colorOptions: [(String?, String)] = [
        (nil, "None"),
        ("#ef4444", "Red"),
        ("#3b82f6", "Blue"),
        ("#22c55e", "Green"),
        ("#f59e0b", "Amber"),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Create a project")
                    .font(.title2.bold())
                Text("Give related mail and people one clear place to live.")
                    .foregroundStyle(.secondary)
            }

            Form {
                TextField("Name", text: $draft.name)
                    .focused($focusedField, equals: .name)
                    .onSubmit(create)

                Picker("Status", selection: $draft.status) {
                    ForEach(statusOptions, id: \.self) { status in
                        Text(status.capitalized).tag(status)
                    }
                }

                HStack(spacing: 8) {
                    Text("Colour")
                    Spacer()
                    ForEach(colorOptions, id: \.1) { hex, name in
                        Button {
                            draft.color = hex
                        } label: {
                            Circle()
                                .fill(hex.flatMap { Color(hex: $0) }
                                      ?? Color(nsColor: .tertiaryLabelColor))
                                .frame(width: 20, height: 20)
                                .overlay {
                                    if draft.color == hex {
                                        Circle().stroke(Color.primary, lineWidth: 2)
                                    }
                                }
                        }
                        .buttonStyle(.plain)
                        .help(name)
                    }
                }

                VStack(alignment: .leading, spacing: 6) {
                    Text("Notes")
                    TextEditor(text: $draft.notes)
                        .focused($focusedField, equals: .notes)
                        .frame(minHeight: 90)
                        .border(Color(nsColor: .separatorColor))
                }
            }
            .formStyle(.grouped)

            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(isCreating)
                Button(isCreating ? "Creating…" : "Create Project") { create() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!draft.canCreate || isCreating)
            }
        }
        .padding(22)
        .frame(width: 480, height: 410)
        .interactiveDismissDisabled(isCreating)
        .onAppear { focusedField = .name }
    }

    private func create() {
        guard draft.canCreate, !isCreating else { return }
        isCreating = true
        model.createProject(
            name: draft.trimmedName,
            notes: draft.notes,
            status: draft.status,
            color: draft.color
        ) { projectId in
            guard let projectId else {
                isCreating = false
                return
            }
            onCreated(projectId)
            dismiss()
        }
    }
}

struct KanbanColumnView: View {
    @ObservedObject var model: AppModel
    let status: String
    let label: String
    let cards: [ProjectCard]
    let onSelect: (String) -> Void

    @State private var isTargeted = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(label)
                .font(.headline)
                .padding(10)
            Divider()
            ScrollView {
                LazyVStack(spacing: 6) {
                    ForEach(cards) { card in
                        ProjectCardView(card: card, onSelect: { onSelect(card.id) })
                            .contextMenu {
                                Menu("Move to") {
                                    Button("Active")  { model.moveProject(card.id, to: "active") }
                                    Button("Waiting") { model.moveProject(card.id, to: "waiting") }
                                    Button("Someday") { model.moveProject(card.id, to: "someday") }
                                    Button("Done")    { model.moveProject(card.id, to: "done") }
                                }
                                Divider()
                                Button("Archive", role: .destructive) {
                                    model.archiveProject(card.id)
                                }
                            }
                            .draggable(card.id)
                    }
                }
                .padding(8)
            }
        }
        .frame(minWidth: 180)
        .background(isTargeted ? Color.accentColor.opacity(0.08) : Color.clear)
        .dropDestination(for: String.self) { ids, _ in
            guard let id = ids.first else { return false }
            model.moveProject(id, to: status)
            return true
        } isTargeted: { isTargeted = $0 }
    }
}

struct ProjectCardView: View {
    let card: ProjectCard
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 0) {
                if let color = card.color {
                    Rectangle()
                        .fill(Color(hex: color) ?? .accentColor)
                        .frame(width: 4)
                        .clipShape(RoundedRectangle(cornerRadius: 2))
                }
                VStack(alignment: .leading, spacing: 4) {
                    Text(card.name)
                        .font(.system(.body, weight: .medium))
                        .foregroundStyle(.primary)
                        .lineLimit(2)
                    HStack(spacing: 6) {
                        Text("✉ \(card.emailCount)")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                        Text("👤 \(card.contactCount)")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                        Spacer()
                        Text(relativeDate(card.lastActivityAt))
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
                .padding(8)
            }
        }
        .buttonStyle(.plain)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
        )
    }
}

// MARK: - Hex colour helper

/// Hex string → SwiftUI Color. Returns nil on bad input (so callers can fall back).
extension Color {
    init?(hex: String) {
        let h = hex.hasPrefix("#") ? String(hex.dropFirst()) : hex
        guard h.count == 6, let v = UInt64(h, radix: 16) else { return nil }
        self.init(
            red:   Double((v >> 16) & 0xFF) / 255,
            green: Double((v >>  8) & 0xFF) / 255,
            blue:  Double( v        & 0xFF) / 255
        )
    }
}
