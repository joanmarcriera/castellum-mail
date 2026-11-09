// Castellum - the "Insert Snippet…" picker: a sheet on the
// compose window with a filter field over the account's snippets. Arrows and Return
// choose, Escape cancels; the last row opens the Snippets window.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

/// The pure part of the picker: which snippets match a query, best first.
enum SnippetPickerLogic {
    /// `snippets` ranked against `query` (the fuzzy matcher the command palette uses), on
    /// name and abbreviation; an empty query keeps the library's own (by name) order.
    static func ranked(_ snippets: [SnippetItem], query: String) -> [SnippetItem] {
        let q = query.trimmingCharacters(in: .whitespaces)
        guard !q.isEmpty else { return snippets }
        return snippets
            .compactMap { item -> (SnippetItem, Int)? in
                let name = CommandPaletteMatcher.score(query: q, target: item.name)
                let abbreviation = item.abbreviation.flatMap { CommandPaletteMatcher.score(query: q, target: $0) }
                guard let best = [name, abbreviation].compactMap({ $0 }).max() else { return nil }
                return (item, best)
            }
            .sorted { $0.1 > $1.1 }
            .map(\.0)
    }
}

struct SnippetPickerView: View {
    let snippets: [SnippetItem]
    var onPick: (SnippetItem) -> Void
    var onManage: () -> Void
    var onCancel: () -> Void

    @State private var query = ""
    @State private var highlighted: String?
    @FocusState private var focused: Bool

    private var ranked: [SnippetItem] { SnippetPickerLogic.ranked(snippets, query: query) }

    var body: some View {
        VStack(spacing: 0) {
            TextField("Find a snippet…", text: $query)
                .textFieldStyle(.plain)
                .font(.title3)
                .padding(12)
                .focused($focused)
                .onSubmit { if let item = ranked.first(where: { $0.id == highlighted }) ?? ranked.first { onPick(item) } }
                .onChange(of: query) { _ in highlighted = ranked.first?.id }
            Divider()
            ScrollView {
                LazyVStack(spacing: 0) {
                    if snippets.isEmpty {
                        Text("No snippets yet").foregroundStyle(.secondary).font(.callout).padding(.vertical, 16)
                    } else if ranked.isEmpty {
                        Text("No matching snippets").foregroundStyle(.secondary).font(.callout).padding(.vertical, 16)
                    }
                    ForEach(ranked) { item in
                        HStack {
                            Text(item.name)
                            Spacer(minLength: 12)
                            if let abbreviation = item.abbreviation, !abbreviation.isEmpty {
                                Text(";\(abbreviation)").font(.system(.caption, design: .monospaced)).foregroundStyle(.secondary)
                            }
                        }
                        .padding(.horizontal, 12).padding(.vertical, 8)
                        .contentShape(Rectangle())
                        .background(highlighted == item.id ? Color.accentColor.opacity(0.14) : Color.clear)
                        .onHover { if $0 { highlighted = item.id } }
                        .onTapGesture { onPick(item) }
                        .accessibilityElement(children: .combine)
                    }
                }
            }
            .frame(maxHeight: 280)
            Divider()
            Button("Manage Snippets…", action: onManage)
                .buttonStyle(.plain)
                .foregroundStyle(Color.accentColor)
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(width: 420)
        .onAppear { highlighted = ranked.first?.id; focused = true }
        .onMoveCommand { move($0) }
        .onExitCommand { onCancel() }
    }

    private func move(_ direction: MoveCommandDirection) {
        let ids = ranked.map(\.id)
        guard !ids.isEmpty else { return }
        let index = highlighted.flatMap { ids.firstIndex(of: $0) }
        switch direction {
        case .down: highlighted = ids[min((index ?? -1) + 1, ids.count - 1)]
        case .up: highlighted = ids[max((index ?? ids.count) - 1, 0)]
        default: break
        }
    }
}
