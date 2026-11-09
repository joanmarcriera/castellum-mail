// Castellum - "Tag…" popover (bare key l / ⌥⌘T) (S42).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Mirrors AssignToProjectPopover's shape (search field, checkbox list,
// keyboard navigation, "New …" row) for tags instead of projects. The
// checkbox reflects the PRIMARY selection's keywords (same message the
// "Tag" menu submenu would checkmark against); the actual add/remove
// direction on a multi-message selection is decided by
// `AppModel.toggleTag`'s Apple Mail rule (add if any lacks it).

import SwiftUI

struct TagPickerView: View {
    /// `.apply` toggles a tag on the selection (the `l` picker). `.goTo` (MM-8,
    /// `g l`) opens a tag's message list instead: Return selects the tag, and
    /// there is no create row, no adopt rows and no checkmarks.
    enum Mode { case apply, goTo }

    @ObservedObject var model: AppModel
    var mode: Mode = .apply
    var onDismiss: () -> Void = {}

    @State private var query = ""
    @State private var isCreatingNew = false
    @State private var newName = ""
    @State private var highlightedKeyword: String?
    @FocusState private var focusedField: Field?

    private enum Field: Hashable { case search, newTagName }

    /// Keyword -> whether the primary selection already carries it
    /// (case-insensitive, matching `AppModel.toggleTag`).
    private var primaryKeywords: Set<String> {
        Set((model.selectedMessage?.keywords ?? []).map { $0.lowercased() })
    }

    private var filtered: [TagDefinition] {
        model.tags.filter {
            query.isEmpty || $0.displayName.localizedCaseInsensitiveContains(query)
        }
    }

    /// Server keywords with no local definition — greyed "adopt" rows.
    private var undefinedKeywordsInUse: [KeywordInUse] {
        guard mode == .apply else { return [] }
        let defined = Set(model.tags.map { $0.keywordKey })
        return model.keywordsInUse.filter {
            !defined.contains($0.keywordKey)
                && (query.isEmpty || $0.keyword.localizedCaseInsensitiveContains(query))
        }
    }

    private var keyboardChoices: [String] {
        filtered.map(\.keyword) + undefinedKeywordsInUse.map(\.keyword)
    }

    var body: some View {
        VStack(spacing: 0) {
            TextField(mode == .goTo ? "Go to tag…" : "Filter or name a tag…", text: $query)
                .textFieldStyle(.roundedBorder)
                .padding(8)
                .focused($focusedField, equals: .search)
                .onSubmit { activateHighlighted() }
                .onChange(of: query) { _ in highlightedKeyword = keyboardChoices.first }
            Divider()
            ScrollView {
                LazyVStack(spacing: 0) {
                    if filtered.isEmpty && undefinedKeywordsInUse.isEmpty && !isCreatingNew {
                        Text(query.isEmpty ? "No tags yet" : "No matching tags")
                            .foregroundStyle(.secondary)
                            .font(.caption)
                            .padding(.vertical, 12)
                    } else {
                        ForEach(filtered) { tag in row(for: tag) }
                        ForEach(undefinedKeywordsInUse) { adoptRow(for: $0) }
                    }
                }
            }
            .frame(maxHeight: 220)
            Divider()
            if mode == .goTo {
                EmptyView()
            } else if isCreatingNew {
                HStack {
                    TextField("Tag name", text: $newName)
                        .textFieldStyle(.roundedBorder)
                        .focused($focusedField, equals: .newTagName)
                        .onSubmit { createAndApply() }
                    Button("Create") { createAndApply() }
                        .disabled(newName.trimmingCharacters(in: .whitespaces).isEmpty)
                        .keyboardShortcut(.defaultAction)
                    Button("Cancel") { isCreatingNew = false; newName = ""; focusedField = .search }
                        .keyboardShortcut(.cancelAction)
                }
                .padding(8)
            } else {
                Button {
                    isCreatingNew = true
                    newName = query
                    DispatchQueue.main.async { focusedField = .newTagName }
                } label: {
                    Label("New Tag…", systemImage: "plus")
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(Color.accentColor)
            }
            if mode == .apply, let notice = model.lastTagLocalOnlyNotice {
                Divider()
                Text(notice)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(8)
            }
        }
        .frame(width: 260)
        .onAppear {
            if mode == .apply { model.reloadKeywordsInUse() }
            highlightedKeyword = keyboardChoices.first
            focusedField = .search
        }
        .onMoveCommand { direction in
            guard !isCreatingNew else { return }
            let choices = keyboardChoices
            guard !choices.isEmpty else { return }
            guard let current = highlightedKeyword, let index = choices.firstIndex(of: current) else {
                highlightedKeyword = direction == .up ? choices.last : choices.first
                return
            }
            switch direction {
            case .down: highlightedKeyword = choices[min(index + 1, choices.count - 1)]
            case .up: highlightedKeyword = choices[max(index - 1, 0)]
            default: break
            }
        }
    }

    /// The digit key that toggles `tag` (1...9, Settings order), or nil beyond the ninth.
    private func slotDigit(of tag: TagDefinition) -> Int? {
        guard let i = model.tags.firstIndex(where: { $0.keywordKey == tag.keywordKey }), i < 9 else { return nil }
        return i + 1
    }

    private func choose(_ tag: TagDefinition) {
        if mode == .goTo {
            model.select(tag: tag)
            onDismiss()
        } else {
            model.toggleTag(tag.keyword)
        }
    }

    private func row(for tag: TagDefinition) -> some View {
        let checked = mode == .apply && primaryKeywords.contains(tag.keyword.lowercased())
        return Button {
            choose(tag)
        } label: {
            HStack {
                Circle().fill(Color(nsColor: TagColour.named(tag.colour).nsColor)).frame(width: 10, height: 10)
                Text(tag.displayName).foregroundStyle(.primary)
                Spacer()
                if mode == .apply, let digit = slotDigit(of: tag) {
                    // The key that toggles this tag without opening the picker.
                    Text("\(digit)")
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
                if checked {
                    Image(systemName: "checkmark").foregroundStyle(Color.accentColor)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .contentShape(Rectangle())
            .background(highlightedKeyword == tag.keyword ? Color.accentColor.opacity(0.14) : Color.clear)
        }
        .buttonStyle(.plain)
        .onHover { if $0 { highlightedKeyword = tag.keyword } }
    }

    private func adoptRow(for keywordInUse: KeywordInUse) -> some View {
        Button {
            model.createTag(
                displayName: keywordInUse.keyword,
                colour: TagColour.next(after: model.tags.count).rawValue,
                keyword: keywordInUse.keyword)
            model.toggleTag(keywordInUse.keyword)
        } label: {
            HStack {
                Circle().strokeBorder(Color.secondary, lineWidth: 1).frame(width: 10, height: 10)
                Text(keywordInUse.keyword).foregroundStyle(.secondary)
                Spacer()
                Text("Adopt").font(.caption).foregroundStyle(Color.accentColor)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func activateHighlighted() {
        if mode == .goTo {
            // Never creates: Return with no match does nothing.
            if let tag = TagGoToMatcher.tag(highlighted: highlightedKeyword, query: query, in: model.tags) {
                choose(tag)
            }
            return
        }
        guard let keyword = highlightedKeyword ?? keyboardChoices.first else {
            if !query.trimmingCharacters(in: .whitespaces).isEmpty {
                isCreatingNew = true
                newName = query
            }
            return
        }
        model.toggleTag(keyword)
    }

    private func createAndApply() {
        let name = newName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return }
        guard let created = model.createTag(
            displayName: name, colour: TagColour.next(after: model.tags.count).rawValue)
        else { return }
        model.toggleTag(created.keyword)
        isCreatingNew = false
        newName = ""
        onDismiss()
    }
}

/// Which tag Return picks in the `g l` go-to picker (MM-8). Pure so it is
/// unit-tested: the highlighted row when it still matches the filter, else the
/// first match; nil when nothing matches (the picker never creates a tag).
enum TagGoToMatcher {
    static func matches(query: String, in tags: [TagDefinition]) -> [TagDefinition] {
        tags.filter { query.isEmpty || $0.displayName.localizedCaseInsensitiveContains(query) }
    }

    static func tag(highlighted: String?, query: String, in tags: [TagDefinition]) -> TagDefinition? {
        let found = matches(query: query, in: tags)
        return found.first { $0.keyword == highlighted } ?? found.first
    }
}
