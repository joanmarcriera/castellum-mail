// Castellum - the after-Send "Undo" banner (S25 undo-send).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// UndoSendBannerState is pure (no SwiftUI, no Foundation clock) so its
// transitions are unit-tested directly, same discipline as AiConnectionTestState.
// UndoSendBannerView is the thin, non-modal presentation: a bottom banner in
// the main window with a live countdown and an Undo button, shown while a
// delayed send's window is still open. It is never a sheet/alert — Send must
// not block the person from doing anything else while the window ticks down.

import SwiftUI

/// State for the single Undo-send banner shown in the main window. At most one
/// delayed send is tracked at a time: queuing a second while one is still
/// counting down REPLACES the banner (its own drain keeps running in the
/// background — only the earlier banner disappears), matching how Superhuman/
/// MailMate-style undo toasts stack-and-replace rather than queue.
struct UndoSendBannerState: Equatable {
    struct Pending: Equatable {
        let draftId: Int64
        let accountId: String
        let totalSeconds: Int
        var remainingSeconds: Int
    }

    private(set) var pending: Pending?

    var isVisible: Bool { pending != nil }

    /// A draft was just queued with an undo window: show the banner.
    /// `delaySeconds <= 0` is a no-op (Undo send Off never shows a banner).
    mutating func send(draftId: Int64, accountId: String, delaySeconds: Int) {
        guard delaySeconds > 0 else { return }
        pending = Pending(
            draftId: draftId, accountId: accountId,
            totalSeconds: delaySeconds, remainingSeconds: delaySeconds)
    }

    /// One second elapsed. Once the countdown reaches zero the banner is
    /// dismissed on its own — the drain (not the UI) owns delivery from here.
    mutating func tick() {
        guard var p = pending else { return }
        p.remainingSeconds -= 1
        pending = p.remainingSeconds > 0 ? p : nil
    }

    /// Explicit dismissal: Undo pressed, or the banner's own close button.
    mutating func dismiss() {
        pending = nil
    }
}

/// The non-modal banner itself. Presentation only — all state lives in
/// `UndoSendBannerState`, owned by `AppModel`.
struct UndoSendBannerView: View {
    let pending: UndoSendBannerState.Pending
    let onUndo: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "paperplane.fill")
                .foregroundStyle(.secondary)
            Text("Message sent")
            Spacer()
            Text("\(pending.remainingSeconds)s")
                .font(.caption)
                .monospacedDigit()
                .foregroundStyle(.secondary)
            Button("Undo", action: onUndo)
                .keyboardShortcut("z", modifiers: .command)
            Button {
                onDismiss()
            } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Dismiss")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.bar)
        .overlay(alignment: .top) { Divider() }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Message sent, \(pending.remainingSeconds) seconds left to undo")
    }
}

/// The "too late to undo" notice: Undo lost the race (the sender already
/// claimed the op) — a short, non-modal explanation in the same bottom slot
/// `UndoSendBannerView` uses (never shown at the same time as it, since
/// `AppModel.undoBannerSend` dismisses the banner before it can appear).
/// Self-dismisses after a few seconds, same as it never being a sheet/alert
/// the person has to act on.
struct UndoTooLateNoticeView: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "clock.badge.exclamationmark")
                .foregroundStyle(.secondary)
            Text(message)
            Spacer()
            Button {
                onDismiss()
            } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Dismiss")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.bar)
        .overlay(alignment: .top) { Divider() }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(message)
        .task {
            try? await Task.sleep(nanoseconds: 4_000_000_000)
            onDismiss()
        }
    }
}
