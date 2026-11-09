// Castellum - the one presenter for every write-safety dialog.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// WHY THIS IS APPKIT AND NOT A SwiftUI `.alert`
// The refusal notice and the enable/disable confirmations used to be a legacy
// `.alert(item:)` on `RootView`. On 2026-09-23 a second `.alert(item:)` was
// added to the same view (compose-unavailable, #138); with that API the later
// modifier replaces the earlier one, so from then on `AppModel.writeSafetyPrompt`
// was set and NOTHING was ever shown: Send from compose on a read-only account
// left only an inline caption, and the sidebar badge did nothing either. Marc
// hit it with a real Outlook.com account on 2026-10-05 and could not leave
// evaluation mode at all. Reproduced by scripts/ui-scenarios/
// ws1-refused-send-enable-writes.json (prompt pending, no window had a sheet).
//
// Even when it worked, that alert lived on the main window: a refusal raised
// from a compose window opened it behind the window the user was looking at.
//
// So the dialog is owned here instead: this object watches the model's single
// prompt slot and shows an `NSAlert` as a sheet on the window the user acted in
// (the key window when the prompt is raised: compose, Settings, a standalone
// message, or main). It cannot be shadowed by a view modifier, and no view
// needs to remember to host it.
//
// USAGE: created once in `AppDelegate.modelBecameAvailable`. Never created
// under XCTest (the model is then driven directly through
// `resolveWriteSafetyPrompt`), so unit tests never put a modal on screen.
//
// SAFETY: the only thing a click can do is call
// `AppModel.resolveWriteSafetyPrompt` with the action of the button clicked.
// Enabling is never the default button (see `AccountWritePolicy.alertSpec`),
// and a dialog whose prompt has since been replaced resolves nothing.

import AppKit
import Combine

@MainActor
final class WriteSafetyPresenter {
    private weak var model: AppModel?
    private var subscription: AnyCancellable?
    /// True while a dialog is on screen: one at a time, like the slot itself.
    private var isPresenting = false

    init(model: AppModel) {
        self.model = model
        // `@Published` fires in willSet, and a compose window registers its
        // retry right after the refusal is raised: hop one main-queue turn so
        // the dialog is built from the settled state.
        subscription = model.$writeSafetyPrompt
            .receive(on: DispatchQueue.main)
            .sink { [weak self] _ in
                MainActor.assumeIsolated { self?.presentIfNeeded() }
            }
    }

    private func presentIfNeeded() {
        guard !isPresenting, let model, let prompt = model.writeSafetyPrompt else { return }
        isPresenting = true
        let spec = AccountWritePolicy.alertSpec(for: prompt, willRetry: model.writeSafetyWillRetry)
        let alert = Self.makeAlert(spec)

        let finish: (NSApplication.ModalResponse) -> Void = { [weak self, weak model] response in
            MainActor.assumeIsolated {
                self?.isPresenting = false
                // Buttons answer 1000, 1001, … in the order they were added.
                let index = response.rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
                let action = spec.buttons.indices.contains(index) ? spec.buttons[index].action : .dismiss
                // A prompt replaced while this dialog was up is not the one the
                // user answered: act only on the prompt that was shown.
                if let model, model.writeSafetyPrompt == prompt {
                    model.resolveWriteSafetyPrompt(action)
                }
                // Anything raised meanwhile (or a follow-up notice) is next.
                DispatchQueue.main.async { self?.presentIfNeeded() }
            }
        }

        if let host = Self.hostWindow() {
            alert.beginSheetModal(for: host, completionHandler: finish)
        } else {
            // No usable window (all closed, or each already has a sheet): the
            // dialog must still appear, so fall back to an app-modal alert.
            finish(alert.runModal())
        }
    }

    /// The window the user acted in: the key window when it can take a sheet,
    /// else the main window, else any ordinary visible window.
    static func hostWindow() -> NSWindow? {
        let candidates = [NSApp.keyWindow, NSApp.mainWindow] + NSApp.orderedWindows.map(Optional.some)
        return candidates.compactMap { $0 }.first { canHostSheet($0) }
    }

    static func canHostSheet(_ window: NSWindow) -> Bool {
        window.isVisible && !window.isMiniaturized
            && window.sheetParent == nil && window.attachedSheet == nil
            && window.styleMask.contains(.titled) && !(window is NSPanel)
    }

    static func makeAlert(_ spec: WriteSafetyAlertSpec) -> NSAlert {
        let alert = NSAlert()
        alert.messageText = spec.title
        alert.informativeText = spec.message
        alert.alertStyle = spec.isWarning ? .warning : .informational
        for button in spec.buttons {
            let added = alert.addButton(withTitle: button.title)
            added.hasDestructiveAction = button.isDestructive
            // NSAlert makes the FIRST button the default; the spec decides
            // instead, so Return never enables writes by accident.
            added.keyEquivalent = button.isDefault ? "\r" : ""
            if button.action == .dismiss && !button.isDefault { added.keyEquivalent = "\u{1b}" }
        }
        return alert
    }
}
