// Castellum - Split Inbox (HP-2) tab strip above the message list.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// One tab per split (sidebar order) and Other last, each with its unread
// count. Shown only while an Inbox is listed and at least one split exists
// (`AppModel.isInboxSplitStripVisible`). Switching tabs: click, or
// View > Next Split / Previous Split (⇧⌘] / ⇧⌘[).
import SwiftUI

/// Badge text for a tab's unread count; `nil` hides it. Pure so it is
/// unit-tested rather than eyeballed.
enum InboxSplitBadge {
    static func label(unread: Int64) -> String? { unread > 0 ? "\(unread)" : nil }
}

struct InboxSplitTabStrip: View {
    @ObservedObject var model: AppModel

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 4) {
                ForEach(model.inboxSplitTabs) { tab in
                    tabButton(tab)
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
        }
        .background(.bar)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(Text("Split Inbox"))
    }

    private func tabButton(_ tab: InboxSplitCount) -> some View {
        let selected = tab.id == model.selectedInboxSplitId
        return Button { model.select(inboxSplit: tab.id) } label: {
            HStack(spacing: 5) {
                Text(tab.displayName)
                    .fontWeight(selected ? .semibold : .regular)
                    .lineLimit(1)
                if let badge = InboxSplitBadge.label(unread: tab.unread) {
                    Text(badge)
                        .font(.caption.monospacedDigit())
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(selected ? Color.accentColor.opacity(0.25) : Color.secondary.opacity(0.18)))
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 4)
            .background(
                RoundedRectangle(cornerRadius: 6)
                    .fill(selected ? Color.accentColor.opacity(0.15) : Color.clear))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(Text("\(tab.displayName): \(tab.unread) unread of \(tab.total)"))
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
