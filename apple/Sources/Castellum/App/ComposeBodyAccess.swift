// Castellum - reach the compose body's NSTextView.
//
// The body is a SwiftUI `TextEditor` with no selection API on the macOS 13
// target, so inserting a snippet at the caret goes through the NSTextView
// underneath it, found the same way `MarkdownSourceTyping` finds it.
// `insertText(_:replacementRange:)` is used (not a string splice) so Cmd-Z
// removes the snippet in one step and the SwiftUI binding is updated by the
// text view's own delegate.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI

/// Weak handle to one compose window's body text view, filled by `ComposeBodyProbe`.
@MainActor
final class ComposeBodyHandle {
    weak var textView: NSTextView?

    /// Insert `rendered` at the caret, replacing the selection, clamped out of a signature
    /// or quote (`SnippetInsertion.insertionRange`), and put the caret at `${cursor}`.
    /// Returns false when the text view is not available: the caller then edits the string.
    @discardableResult
    func insert(_ rendered: RenderedTemplate) -> Bool {
        guard let tv = textView else { return false }
        let range = SnippetInsertion.insertionRange(in: tv.string, selection: tv.selectedRange())
        guard tv.shouldChangeText(in: range, replacementString: rendered.text) else { return true }
        tv.insertText(rendered.text, replacementRange: range)
        let caret = range.location + (rendered.caretOffset ?? rendered.text.utf16.count)
        tv.setSelectedRange(NSRange(location: min(caret, (tv.string as NSString).length), length: 0))
        tv.window?.makeFirstResponder(tv)
        return true
    }
}

extension ComposeBodyHandle {
    /// Slice 3: if the text just before the caret is `;abbrev` naming one of `items`, replace
    /// it with `render(item)` and put the caret at its `${cursor}`. Returns whether it did
    /// (the Tab key is only consumed then). Never acts on a selection, and never when the
    /// text view is not available.
    @discardableResult
    func expandInlineSnippet(items: [SnippetItem], render: (SnippetItem) -> RenderedTemplate) -> Bool {
        guard let tv = textView, tv.selectedRange().length == 0 else { return false }
        let caret = tv.selectedRange().location
        let before = (tv.string as NSString).substring(to: min(caret, (tv.string as NSString).length))
        guard let hit = SnippetTrigger.match(textBeforeCaret: before, in: items) else { return false }
        let rendered = render(hit.item)
        guard tv.shouldChangeText(in: hit.replace, replacementString: rendered.text) else { return true }
        tv.insertText(rendered.text, replacementRange: hit.replace)
        let newCaret = hit.replace.location + (rendered.caretOffset ?? rendered.text.utf16.count)
        tv.setSelectedRange(NSRange(location: min(newCaret, (tv.string as NSString).length), length: 0))
        return true
    }
}

/// Tab expansion for `;abbrev`, scoped to ONE compose window: a local key monitor that takes
/// Tab only when this window is key, the body text view is first responder, the switch is on
/// and `expandInlineSnippet` finds a match. Every other Tab passes through untouched.
private struct SnippetTriggerMonitor: NSViewRepresentable {
    let session: ComposeSession
    let model: AppModel
    let prefs: Preferences

    func makeNSView(context: Context) -> NSView { NSView(frame: .zero) }

    func updateNSView(_ view: NSView, context: Context) {
        context.coordinator.session = session
        context.coordinator.model = model
        context.coordinator.prefs = prefs
        context.coordinator.hostView = view
        context.coordinator.installIfNeeded()
    }

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) { coordinator.remove() }

    func makeCoordinator() -> Coordinator { Coordinator() }

    @MainActor
    final class Coordinator {
        var session: ComposeSession?
        var model: AppModel?
        var prefs: Preferences?
        weak var hostView: NSView?
        private var monitor: Any?

        func installIfNeeded() {
            guard monitor == nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
                guard let self, self.handle(event) else { return event }
                return nil
            }
        }

        func remove() {
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
        }

        private func handle(_ event: NSEvent) -> Bool {
            guard event.keyCode == 48,   // Tab
                  event.modifierFlags.intersection(.deviceIndependentFlagsMask).isEmpty,
                  let prefs, prefs.templateLibrary.inlineTriggerEnabled,
                  let session, let model,
                  let window = hostView?.window, window.isKeyWindow,
                  let tv = session.bodyHandle.textView, window.firstResponder === tv,
                  !session.queuedInOutbox, !session.isSending else { return false }
            let items = prefs.templateLibrary.snippets(for: session.accountId)
            return session.bodyHandle.expandInlineSnippet(items: items) { item in
                let context = session.snippetContext(
                    selfEmail: model.selfEmail(for: session.accountId),
                    selfName: model.accounts.first { $0.id == session.accountId }?.displayName)
                return TemplateFormatter.render(item.text, context: context, mode: .multiLine)
            }
        }
    }
}

/// Zero-size probe that stores the TextEditor's NSTextView in a `ComposeBodyHandle`.
private struct ComposeBodyProbe: NSViewRepresentable {
    let handle: ComposeBodyHandle

    func makeNSView(context: Context) -> NSView { NSView(frame: .zero) }

    func updateNSView(_ view: NSView, context: Context) {
        let handle = handle
        // The text view may not be in the hierarchy yet on the first pass: look now and once more next turn.
        func attempt() {
            if handle.textView == nil { handle.textView = MarkdownSourceTextFlags.nearestTextView(from: view) }
        }
        attempt()
        DispatchQueue.main.async { attempt() }
    }
}

extension View {
    /// Slice 3: `;abbrev` + Tab expansion for this compose window (off unless switched on in Snippets).
    func snippetTabExpansion(session: ComposeSession, model: AppModel, prefs: Preferences) -> some View {
        background(SnippetTriggerMonitor(session: session, model: model, prefs: prefs).frame(width: 0, height: 0))
    }

    /// Keep `handle` pointing at this TextEditor's NSTextView.
    func composeBodyHandle(_ handle: ComposeBodyHandle) -> some View {
        background(ComposeBodyProbe(handle: handle).frame(width: 0, height: 0))
    }
}
