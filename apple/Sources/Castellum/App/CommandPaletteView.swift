// Castellum - the command palette UI (S45): fuzzy-filtered, keyboard-only
// list over CommandRegistry, following AssignToProjectPopover's proven
// notification-triggered-overlay/autofocused-TextField/onMoveCommand shape
// (AssignToProjectPopover.swift) rather than a new AppKit NSSearchField —
// see this plan's Assumption 3.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

/// Dismiss-then-post, mirroring `AssignToProjectPopover.beginCreating()`'s
/// `DispatchQueue.main.async` idiom for a responder-timing issue: the
/// palette's own TextField holds first responder while a row is chosen, and
/// `CommandDispatcher` (`CommandRegistry.swift`) blocks every DESTRUCTIVE
/// command while a text editor owns focus (⌘⌫ mid-typing must not delete a
/// message). Posting on the exact same run-loop turn as `dismiss()` risks
/// the window's first responder not yet having moved off the sheet's field,
/// which would make a palette-launched Archive or Delete silently no-op.
/// Injectable for tests; production uses `NotificationCenter.default.post`.
@MainActor
struct CommandPaletteExecutor {
    var dismiss: () -> Void
    var post: (Notification.Name) -> Void = {
        NotificationCenter.default.post(name: $0, object: nil)
    }

    func execute(_ spec: CommandSpec) {
        guard let notification = spec.notification else { return }
        dismiss()
        DispatchQueue.main.async {
            post(notification)
        }
    }
}

struct CommandPaletteView: View {
    @ObservedObject var model: AppModel
    var onDismiss: () -> Void

    @State private var query = ""
    @State private var highlightedId: CommandID?
    /// Row to scroll into view. Set only by keyboard moves and query changes,
    /// never by hover, so the pointer cannot fight the user's own scrolling.
    @State private var scrollTarget: CommandID?
    @FocusState private var focused: Bool

    /// Every dispatchable command — see Assumption 5 (a command with no
    /// `notification` is local-only dispatch and already has a dedicated
    /// key: j/k/Return/double-click/⌘⏎).
    private var eligible: [CommandSpec] {
        model.keyBindings.effectiveSpecs.filter { $0.notification != nil && !CommandRegistry.paletteExcluded.contains($0.id) }.compactMap { spec in
            // MM-8: a tag slot reads "Toggle Tag 1: Work", and an empty slot is not listed.
            guard let n = spec.id.tagSlotIndex else { return spec }
            guard n <= model.tags.count else { return nil }
            return spec.retitled("Toggle Tag \(n): \(model.tags[n - 1].displayName)")
        }
    }

    private var ranked: [CommandSpec] {
        CommandPaletteMatcher.rank(query: query, commands: eligible)
    }

    private var context: PaletteContext { model.paletteContext }

    var body: some View {
        VStack(spacing: 0) {
            TextField("Type a command…", text: $query)
                .textFieldStyle(.plain)
                .font(.title3)
                .padding(12)
                .focused($focused)
                .onSubmit { activateHighlighted() }
                .onChange(of: query) { _ in
                    highlightedId = ranked.first?.id
                    scrollTarget = highlightedId
                }
            Divider()
            // ScrollViewReader keeps the keyboard-highlighted row on screen:
            // without it ↓/↑ moved the highlight past the 360pt viewport and
            // Return ran a command the user could not see (#3061).
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 0) {
                        if ranked.isEmpty {
                            Text("No matching commands")
                                .foregroundStyle(.secondary)
                                .font(.callout)
                                .padding(.vertical, 16)
                        } else {
                            ForEach(ranked, id: \.id) { spec in row(spec).id(spec.id) }
                        }
                    }
                }
                .onChange(of: scrollTarget) { id in
                    guard let id else { return }
                    proxy.scrollTo(id)
                }
            }
            .frame(maxHeight: 360)
        }
        .frame(width: 480)
        .onAppear {
            highlightedId = ranked.first?.id
            focused = true
        }
        .onMoveCommand { move($0) }
        .onExitCommand { onDismiss() }
    }

    private func row(_ spec: CommandSpec) -> some View {
        let enabled = CommandPaletteAvailability.isEnabled(spec.id, context: context)
        return HStack {
            Text(spec.help ?? spec.title)
                .foregroundStyle(enabled ? .primary : .secondary)
            Spacer(minLength: 12)
            if let keycap = spec.sheetKeys.first {
                Text(keycap)
                    .font(.system(.caption, design: .monospaced))
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(
                        RoundedRectangle(cornerRadius: 4)
                            .fill(Color(nsColor: .controlBackgroundColor)))
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .contentShape(Rectangle())
        .background(highlightedId == spec.id ? Color.accentColor.opacity(0.14) : Color.clear)
        .opacity(enabled ? 1 : 0.45)
        .onHover { isHovering in if isHovering, enabled { highlightedId = spec.id } }
        .onTapGesture {
            guard enabled else { return }
            highlightedId = spec.id
            activateHighlighted()
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            enabled
                ? (spec.help ?? spec.title)
                : "\(spec.help ?? spec.title), \(String(localized: "unavailable"))"
        )
    }

    private func move(_ direction: MoveCommandDirection) {
        let enabledRanked = ranked.filter { CommandPaletteAvailability.isEnabled($0.id, context: context) }
        guard !enabledRanked.isEmpty else { return }
        guard let current = highlightedId,
              let index = enabledRanked.firstIndex(where: { $0.id == current }) else {
            highlightedId = direction == .up ? enabledRanked.last?.id : enabledRanked.first?.id
            scrollTarget = highlightedId
            return
        }
        switch direction {
        case .down: highlightedId = enabledRanked[min(index + 1, enabledRanked.count - 1)].id
        case .up: highlightedId = enabledRanked[max(index - 1, 0)].id
        default: return
        }
        scrollTarget = highlightedId
    }

    private func activateHighlighted() {
        guard let id = highlightedId,
              let spec = ranked.first(where: { $0.id == id }),
              CommandPaletteAvailability.isEnabled(id, context: context) else { return }
        CommandPaletteExecutor(dismiss: onDismiss).execute(spec)
    }
}
