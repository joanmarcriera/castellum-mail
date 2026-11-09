// Castellum - "Assign to project" popover (bare key p / ⇧⌘P).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

enum ProjectAssignmentChoice: Hashable {
    case project(String)
    case createProject
}

enum ProjectAssignmentKeyboardNavigation {
    static func choices(projectIds: [String]) -> [ProjectAssignmentChoice] {
        projectIds.map(ProjectAssignmentChoice.project) + [.createProject]
    }

    static func move(
        from current: ProjectAssignmentChoice?,
        direction: MoveCommandDirection,
        choices: [ProjectAssignmentChoice]
    ) -> ProjectAssignmentChoice? {
        guard !choices.isEmpty else { return nil }
        guard let current, let index = choices.firstIndex(of: current) else {
            return direction == .up ? choices.last : choices.first
        }
        switch direction {
        case .down:
            return choices[min(index + 1, choices.count - 1)]
        case .up:
            return choices[max(index - 1, 0)]
        default:
            return current
        }
    }
}

struct AssignToProjectPopover: View {
    @ObservedObject var model: AppModel
    let messageIds: [String]
    var onDismiss: () -> Void = {}

    @State private var query = ""
    @State private var memberProjectIds: Set<String> = []
    @State private var isCreatingNew = false
    @State private var newName = ""
    @State private var highlightedChoice: ProjectAssignmentChoice?
    @FocusState private var focusedField: Field?

    private enum Field: Hashable { case search, newProjectName }

    private var filtered: [ProjectCard] {
        model.projects.filter {
            !$0.archived
                && (query.isEmpty || $0.name.localizedCaseInsensitiveContains(query))
        }
    }

    private var keyboardChoices: [ProjectAssignmentChoice] {
        ProjectAssignmentKeyboardNavigation.choices(projectIds: filtered.map(\.id))
    }

    var body: some View {
        VStack(spacing: 0) {
            TextField("Search projects…", text: $query)
                .textFieldStyle(.roundedBorder)
                .padding(8)
                .focused($focusedField, equals: .search)
                .onSubmit { activateHighlightedChoice() }
                .onChange(of: query) { _ in
                    highlightedChoice = keyboardChoices.first
                }
            Divider()
            ScrollView {
                LazyVStack(spacing: 0) {
                    if filtered.isEmpty && !isCreatingNew {
                        Text(query.isEmpty ? "No projects yet" : "No matching projects")
                            .foregroundStyle(.secondary)
                            .font(.caption)
                            .padding(.vertical, 12)
                    } else {
                        ForEach(filtered, id: \.id) { (card: ProjectCard) in
                            Button {
                                toggleLink(card.id)
                            } label: {
                                HStack {
                                    if let color = card.color {
                                        Circle()
                                            .fill(Color(hex: color) ?? Color.accentColor)
                                            .frame(width: 10, height: 10)
                                    }
                                    Text(card.name).foregroundStyle(.primary)
                                    Spacer()
                                    if memberProjectIds.contains(card.id) {
                                        Image(systemName: "checkmark").foregroundStyle(Color.accentColor)
                                    }
                                }
                                .padding(.horizontal, 12)
                                .padding(.vertical, 8)
                                .contentShape(Rectangle())
                                .background(
                                    highlightedChoice == .project(card.id)
                                        ? Color.accentColor.opacity(0.14)
                                        : Color.clear
                                )
                            }
                            .buttonStyle(.plain)
                            .onHover { isHovering in
                                if isHovering { highlightedChoice = .project(card.id) }
                            }
                            Divider()
                        }
                    }
                }
            }
            .frame(maxHeight: 220)
            Divider()
            if isCreatingNew {
                HStack {
                    TextField("Project name", text: $newName)
                        .textFieldStyle(.roundedBorder)
                        .focused($focusedField, equals: .newProjectName)
                        .onSubmit { createAndLink() }
                    Button("Create") { createAndLink() }
                        .disabled(newName.trimmingCharacters(in: .whitespaces).isEmpty)
                        .keyboardShortcut(.defaultAction)
                    Button("Cancel") { cancelCreating() }
                        .keyboardShortcut(.cancelAction)
                }
                .padding(8)
            } else {
                Button { beginCreating() } label: {
                    Label("New Project…", systemImage: "plus")
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                        .contentShape(Rectangle())
                        .background(
                            highlightedChoice == .createProject
                                ? Color.accentColor.opacity(0.14)
                                : Color.clear
                        )
                }
                .buttonStyle(.plain)
                .foregroundStyle(Color.accentColor)
                .onHover { isHovering in
                    if isHovering { highlightedChoice = .createProject }
                }
            }
        }
        .frame(width: 240)
        .onAppear {
            loadMemberships()
            highlightedChoice = keyboardChoices.first
            focusedField = .search
        }
        .onChange(of: filtered.map(\.id)) { _ in
            let choiceIsVisible = highlightedChoice.map { keyboardChoices.contains($0) } ?? false
            if !choiceIsVisible
                || (highlightedChoice == .createProject && !filtered.isEmpty) {
                highlightedChoice = keyboardChoices.first
            }
        }
        .onMoveCommand { direction in
            guard !isCreatingNew else { return }
            highlightedChoice = ProjectAssignmentKeyboardNavigation.move(
                from: highlightedChoice,
                direction: direction,
                choices: keyboardChoices
            )
        }
    }

    // MARK: - helpers

    private func loadMemberships() {
        model.reloadProjects(includeArchived: false)
        // Checkmark = EVERY selected message is in that project (intersection),
        // so toggling always has one predictable meaning for the whole selection.
        model.projectMembershipIntersection(for: messageIds) { common in
            memberProjectIds = common
        }
    }

    private func toggleLink(_ projectId: String) {
        let linked = !memberProjectIds.contains(projectId)
        memberProjectIds.formSymmetricDifference([projectId])
        model.assignMessagesToProject(projectId: projectId, linked: linked,
                                      messageIds: messageIds)
        onDismiss()
    }

    private func activateHighlightedChoice() {
        let choice = highlightedChoice ?? keyboardChoices.first
        switch choice {
        case .project(let projectId):
            toggleLink(projectId)
        case .createProject:
            beginCreating()
        case nil:
            break
        }
    }

    private func beginCreating() {
        isCreatingNew = true
        highlightedChoice = .createProject
        DispatchQueue.main.async { focusedField = .newProjectName }
    }

    private func cancelCreating() {
        isCreatingNew = false
        newName = ""
        focusedField = .search
    }

    private func createAndLink() {
        let name = newName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return }
        model.createProjectAndAssign(name: name, messageIds: messageIds) { newId in
            memberProjectIds.insert(newId)
            onDismiss()
        }
    }
}
