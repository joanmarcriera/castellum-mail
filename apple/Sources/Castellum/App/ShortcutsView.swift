// Castellum - keyboard shortcuts cheat sheet (press ? in the message list).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct ShortcutsView: View {
    @Environment(\.dismiss) private var dismiss

    struct Shortcut: Identifiable {
        let id = UUID()
        /// Alternative key sequences, rendered as separate keycaps
        /// ("J" and "↓" rather than one cramped "J / ↓" string).
        let keys: [String]
        let action: String

        init(_ keys: [String], _ action: String) {
            self.keys = keys
            self.action = action
        }
    }

    struct Group: Identifiable {
        let id = UUID()
        let title: String
        let symbol: String
        let items: [Shortcut]
    }

    /// Derived from the command registry — the sheet can never drift from
    /// the real bindings again. Passed in by the caller (RootView) using
    /// AppModel's effective (override-applied) specs, so a rebound key shows
    /// up here too. `CommandRegistryTests`/`KeyBindingsStoreTests` pin the
    /// derivation and the override layer respectively.
    let groups: [Group]

    private let columns = [
        GridItem(.flexible(), spacing: 16, alignment: .top),
        GridItem(.flexible(), spacing: 16, alignment: .top),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline) {
                Text("Keyboard Shortcuts")
                    .font(.title2)
                    .bold()
                Spacer()
                Text("Single keys work from the message list")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 20)
            .padding(.top, 18)
            .padding(.bottom, 12)

            ScrollView {
                LazyVGrid(columns: columns, alignment: .leading, spacing: 16) {
                    ForEach(groups) { group in
                        groupCard(group)
                    }
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }

            Divider()
            HStack {
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 12)
        }
        .frame(width: 620, height: 560)
    }

    private func groupCard(_ group: Group) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(group.title, systemImage: group.symbol)
                .font(.subheadline)
                .fontWeight(.semibold)
                .foregroundStyle(.secondary)
                .labelStyle(.titleAndIcon)
            VStack(alignment: .leading, spacing: 6) {
                ForEach(group.items) { item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(item.action)
                            .font(.callout)
                            .fixedSize(horizontal: false, vertical: true)
                        Spacer(minLength: 12)
                        keycaps(item.keys)
                    }
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel(
                        "\(item.action): \(item.keys.joined(separator: " \(String(localized: "or")) "))"
                    )
                }
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(Color(nsColor: .unemphasizedSelectedContentBackgroundColor).opacity(0.45))
        )
    }

    private func keycaps(_ keys: [String]) -> some View {
        HStack(spacing: 4) {
            ForEach(Array(keys.enumerated()), id: \.offset) { index, key in
                if index > 0 {
                    Text("or")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
                Text(key)
                    .font(.system(.caption, design: .monospaced))
                    .fontWeight(.medium)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(
                        RoundedRectangle(cornerRadius: 4)
                            .fill(Color(nsColor: .controlBackgroundColor))
                            .overlay(
                                RoundedRectangle(cornerRadius: 4)
                                    .strokeBorder(Color.secondary.opacity(0.35), lineWidth: 0.5)
                            )
                    )
            }
        }
        .layoutPriority(1)
    }
}
