// Castellum - Settings -> Tags tab (S42).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Two sections: the user's own tag definitions (rename, recolour, drag to
// reorder, delete-the-definition-only) and the read-only "in use on your
// servers" adopt list from `AppModel.keywordsInUse` for keywords a server
// already has that nobody has named locally yet.

import SwiftUI

struct TagsSettingsTab: View {
    @ObservedObject var model: AppModel
    @State private var pendingDeletion: TagDefinition?

    /// The empty-state hint, with the key read from the command registry so it can never
    /// drift from the binding. Shown UPPERCASE: the bare key is lowercase `l`, which in the
    /// system sans font is indistinguishable from capital `I` (: Marc pressed `I`).
    /// Bare keys are case-insensitive for letters, so `L` is the same key.
    static var emptyStateHint: String {
        let key = CommandRegistry.spec(for: .tagMessage).bareKey.map { String($0).uppercased() } ?? "?"
        return String(format: String(localized: "No tags yet. Tag a message (press %@) to create one."), key)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if model.tags.isEmpty {
                Text(Self.emptyStateHint)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                List {
                    ForEach(model.tags) { tag in
                        TagRow(tag: tag, onChange: { model.updateTag($0) },
                               onDelete: { pendingDeletion = tag })
                    }
                    .onMove(perform: moveTags)
                }
            }
            Divider()
            inUseSection
        }
        .onAppear { model.reloadKeywordsInUse() }
        .confirmationDialog(
            "Deleting a tag removes its name and colour from Castellum. Messages keep the keyword on the server.",
            isPresented: Binding(get: { pendingDeletion != nil }, set: { if !$0 { pendingDeletion = nil } }),
            presenting: pendingDeletion
        ) { tag in
            Button("Delete “\(tag.displayName)”", role: .destructive) {
                model.deleteTag(tag)
                pendingDeletion = nil
            }
        }
    }

    private var inUseSection: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("In use on your servers").font(.caption).foregroundStyle(.secondary)
                .padding(.horizontal, 12).padding(.top, 8)
            let defined = Set(model.tags.map { $0.keywordKey })
            let undefined = model.keywordsInUse.filter { !defined.contains($0.keywordKey) }
            if undefined.isEmpty {
                Text("Every server keyword has a local tag.")
                    .font(.caption).foregroundStyle(.tertiary)
                    .padding(.horizontal, 12).padding(.bottom, 8)
            } else {
                ForEach(undefined) { keywordInUse in
                    HStack {
                        Text(keywordInUse.keyword)
                        Spacer()
                        Text("\(keywordInUse.count)").foregroundStyle(.secondary).font(.caption)
                        Button("Add") {
                            _ = model.createTag(
                                displayName: keywordInUse.keyword,
                                colour: TagColour.next(after: model.tags.count).rawValue,
                                keyword: keywordInUse.keyword)
                        }
                    }
                    .padding(.horizontal, 12).padding(.vertical, 4)
                }
            }
        }
    }

    private func moveTags(from source: IndexSet, to destination: Int) {
        var reordered = model.tags
        reordered.move(fromOffsets: source, toOffset: destination)
        for (index, tag) in reordered.enumerated() where tag.sortOrder != Int64(index) {
            var updated = tag
            updated.sortOrder = Int64(index)
            model.updateTag(updated)
        }
    }
}

private struct TagRow: View {
    let tag: TagDefinition
    let onChange: (TagDefinition) -> Void
    let onDelete: () -> Void

    @State private var name: String

    init(tag: TagDefinition, onChange: @escaping (TagDefinition) -> Void, onDelete: @escaping () -> Void) {
        self.tag = tag
        self.onChange = onChange
        self.onDelete = onDelete
        _name = State(initialValue: tag.displayName)
    }

    var body: some View {
        HStack {
            Menu {
                ForEach(TagColour.allCases, id: \.self) { colour in
                    Button {
                        var updated = tag
                        updated.colour = colour.rawValue
                        onChange(updated)
                    } label: {
                        Label(colour.rawValue.capitalized, systemImage: "circle.fill")
                    }
                }
            } label: {
                // An Image, not a Shape: a borderless macOS Menu drops a Shape
                // label (RP09), and a non-template NSImage keeps its colour.
                Label {
                    Text("Tag colour")
                } icon: {
                    Image(nsImage: TagColourDot.image(TagColour.named(tag.colour).nsColor))
                }
                .labelStyle(.iconOnly)
            }
            .menuStyle(.borderlessButton)
            .frame(width: 34)
            .accessibilityLabel(String(localized: "Tag colour"))
            .help(String(localized: "Tag colour"))

            TextField("Name", text: $name, onCommit: {
                let trimmed = name.trimmingCharacters(in: .whitespaces)
                guard !trimmed.isEmpty, trimmed != tag.displayName else {
                    name = tag.displayName
                    return
                }
                var updated = tag
                updated.displayName = trimmed
                onChange(updated)
            })
            .textFieldStyle(.plain)

            if tag.displayName.caseInsensitiveCompare(tag.keyword) != .orderedSame {
                Text("(\(tag.keyword))").font(.caption).foregroundStyle(.secondary)
            }

            Spacer()

            Button(role: .destructive, action: onDelete) {
                Image(systemName: "trash")
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}

/// A small filled dot as a non-template image, so a Menu label shows it in colour (RP09).
enum TagColourDot {
    static func image(_ colour: NSColor, diameter: CGFloat = 12) -> NSImage {
        let image = NSImage(size: NSSize(width: diameter, height: diameter), flipped: false) { rect in
            colour.setFill()
            NSBezierPath(ovalIn: rect).fill()
            return true
        }
        image.isTemplate = false
        return image
    }
}
