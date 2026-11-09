// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import SwiftUI

/// FP29: what the user is told when a send fails AFTER the undo window (the
/// compose window is long gone and the banner said "Message sent"). Pure, so
/// the outcome-to-notice mapping is unit-tested without a core.
///
/// Usage: `SendFailureNotice.message(failed:thrown:lastError:)` returns nil
/// when nothing failed; otherwise "Not sent — kept in Outbox. <reason>". The
/// reason is a readable kind chosen from the error text — raw provider text
/// is never shown.
enum SendFailureNotice {
    static func message(failed: Int, thrown: String?, lastError: String?) -> String? {
        guard thrown != nil || failed > 0 else { return nil }
        let reason = reason(for: thrown ?? lastError ?? "")
        return String(localized: "Not sent — kept in Outbox. \(reason)")
    }

    static func reason(for raw: String) -> String {
        // MS1: checked before the auth words, which its kind token contains.
        if SmtpAuthDisabledGuidance.applies(to: raw) {
            return SmtpAuthDisabledGuidance.headline
        }
        // G8 (#3632): a permanent refusal of the message, not a sign-in.
        if raw.hasPrefix("rejected: ") {
            return String(localized: "The mail server refused the message.")
        }
        let t = raw.lowercased()
        let authWords = ["auth", "credential", "password", "535", "534", "login", "sign-in"]
        let netWords = ["connect", "refused", "timed out", "timeout", "unreachable",
                        "network", "dns", "resolve", "offline", "tls", "certificate"]
        if authWords.contains(where: t.contains) {
            return String(localized: "The mail server refused the sign-in.")
        }
        if netWords.contains(where: t.contains) {
            return String(localized: "The mail server could not be reached.")
        }
        return String(localized: "The mail server did not accept the message.")
    }
}

/// The failed-send notice: same bottom slot as `UndoTooLateNoticeView`, but
/// it stays until dismissed (the user must not miss that a message did not
/// go) and offers "Open Outbox…".
struct SendFailedNoticeView: View {
    let message: String
    let onOpenOutbox: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "exclamationmark.triangle")
                .foregroundStyle(.orange)
            Text(message)
                .uiProbe("sendFailedNotice", role: "staticText", label: message, action: nil)
            Spacer()
            Button("Open Outbox…") { onOpenOutbox() }
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
        .accessibilityElement(children: .contain)
    }
}
