// Castellum - keyboard-driven address completion for To/Cc/Bcc (S47).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Wraps a plain NSTextField in Cocoa's own text-completion mechanism
// (NSTextFieldDelegate.control(_:textView:completions:forPartialWordRange:
// indexOfSelectedItem:)) rather than a hand-rolled popover — arrow-key
// navigation, Tab/Return-to-accept and Esc-to-dismiss come for free, and
// NSTextView's default word-boundary detection already treats ","/";"/
// whitespace as separators, so completion naturally targets the recipient
// segment currently being typed in a comma-separated field. Keeps the
// existing free-text AddressParser/AddressFormatter model unchanged — no
// token/chip UI, no round-trip format change. Deployment target is macOS
// 13.0 (apple/project.yml:9), which rules out SwiftUI's .onKeyPress
// (macOS 14+) — this is why an AppKit wrapper, not a SwiftUI-only control.

import AppKit
import SwiftUI

struct AddressAutocompleteField: NSViewRepresentable {
    @Binding var text: String
    var placeholder: String
    /// Produces completion strings for the CURRENT partial word (the
    /// comma-separated segment under the cursor) — each REPLACES that
    /// segment verbatim, so callers return full "Name <email>" (or bare
    /// email) text, e.g. `AddressCompleter.suggestions(...).map(\.completionText)`.
    var completions: (String) -> [String]
    /// Called once, on the field's first edit — used to lazily request
    /// Contacts permission (`SystemContactsSource.shared.prepareIfNeeded()`)
    /// without ever blocking the field itself.
    var onFirstInteraction: () -> Void = {}

    func makeNSView(context: Context) -> NSTextField {
        let field = NSTextField()
        // `placeholder` arrives already resolved (callers pass String(localized:)).
        field.placeholderString = placeholder
        field.delegate = context.coordinator
        field.usesSingleLineMode = true
        field.lineBreakMode = .byTruncatingTail
        return field
    }

    func updateNSView(_ field: NSTextField, context: Context) {
        if field.stringValue != text { field.stringValue = text }
        context.coordinator.parent = self
    }

    func makeCoordinator() -> Coordinator { Coordinator(parent: self) }

    @MainActor
    final class Coordinator: NSObject, NSTextFieldDelegate {
        var parent: AddressAutocompleteField
        private var hasInteracted = false

        init(parent: AddressAutocompleteField) { self.parent = parent }

        func controlTextDidBeginEditing(_ obj: Notification) {
            guard let field = obj.object as? NSTextField,
                  let editor = field.currentEditor() as? NSTextView else { return }
            // Live-as-you-type suggestions (macOS 12+); Escape (Cocoa's
            // default `complete:` key binding) triggers the same delegate
            // method regardless, so completion still works if this is ever
            // unset by some other code path.
            editor.isAutomaticTextCompletionEnabled = true
        }

        func controlTextDidChange(_ obj: Notification) {
            guard let field = obj.object as? NSTextField else { return }
            parent.text = field.stringValue
            if !hasInteracted {
                hasInteracted = true
                parent.onFirstInteraction()
            }
        }

        func control(
            _ control: NSControl,
            textView: NSTextView,
            completions words: [String],
            forPartialWordRange charRange: NSRange,
            indexOfSelectedItem index: UnsafeMutablePointer<Int>
        ) -> [String] {
            let partial = (textView.string as NSString).substring(with: charRange)
            index.pointee = -1
            return parent.completions(partial)
        }
    }
}
