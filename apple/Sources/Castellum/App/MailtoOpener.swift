// Castellum - opens a prefilled compose window from a mailto:/Services/Share
// source (S57). One entry point so all three surfaces reuse the exact same
// compose-window creation path as ⌘N and Reply (`ComposeWindowController`).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

@MainActor
enum MailtoOpener {
    /// Opens `prefill` in a new compose window on the default account
    /// (`AppModel.composeDefaultAccountId`, the same choice ⌘N uses), with
    /// that account's S26 signature inserted the same way every other
    /// compose window gets one. No network call: this only opens a window.
    static func open(_ prefill: MailtoPrefill, model: AppModel) {
        guard let accountId = model.composeAccountIdOrPromptUnavailable() else { return }
        ComposeWindowController.show(
            model: model,
            session: ComposeSession(
                accountId: accountId, mailto: prefill,
                signature: Preferences.shared.signature(for: accountId)),
            aiCoordinator: model.aiCoordinator)
    }

    /// Parses a raw `mailto:` URI string and opens it, if it parses.
    static func openMailtoLink(_ urlString: String, model: AppModel) {
        guard let prefill = MailtoParser.parse(urlString) else { return }
        open(prefill, model: model)
    }
}
