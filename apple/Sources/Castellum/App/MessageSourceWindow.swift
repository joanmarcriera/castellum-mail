// Castellum - "Message Source" window: the full RFC822 text (headers +
// body), monospaced, selectable and searchable via AppKit's built-in find
// bar (⌘F) — SwiftUI's Text/TextEditor offer neither, so this is a small
// NSViewRepresentable over NSTextView, the same bridging shape as
// EmailBodyWebView/SearchFieldView elsewhere in this app. S49.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI

struct RawSourceTextView: NSViewRepresentable {
    let text: String

    func makeNSView(context: Context) -> NSScrollView {
        let textView = NSTextView()
        textView.isEditable = false
        textView.isSelectable = true
        textView.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.isAutomaticDataDetectionEnabled = false
        textView.isAutomaticLinkDetectionEnabled = false
        textView.usesFindBar = true
        textView.isIncrementalSearchingEnabled = true
        textView.textContainerInset = NSSize(width: 12, height: 12)
        textView.isHorizontallyResizable = true
        textView.textContainer?.widthTracksTextView = false
        textView.textContainer?.containerSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.autoresizingMask = [.width]
        textView.string = text

        let scroll = NSScrollView()
        scroll.documentView = textView
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let textView = scroll.documentView as? NSTextView, textView.string != text else { return }
        textView.string = text
    }
}

/// Reads a cached raw-source file as text. Uses `String(decoding:as:)`
/// (never throws) rather than a UTF-8-only initializer — the wire bytes
/// can legitimately contain non-UTF-8 8bit-encoded parts; invalid
/// sequences become U+FFFD rather than failing the whole view.
func loadRawSourceText(path: String) -> String {
    guard let data = FileManager.default.contents(atPath: path) else {
        return "(couldn't read the cached file at \(path))"
    }
    return String(decoding: data, as: UTF8.self)
}

/// The source window's root view: the text view with the window's minimum
/// size as its own minimum frame, because the hosting controller derives the
/// window's contentMinSize from its root view's minimum (a bare text view has
/// none, which zeroed the window's minimum on first layout).
struct MessageSourceContent: View {
    let text: String

    var body: some View {
        RawSourceTextView(text: text)
            .frame(minWidth: MessageSourceWindow.minimumContentSize.width,
                   minHeight: MessageSourceWindow.minimumContentSize.height)
    }
}

@MainActor
final class MessageSourceWindow: NSObject, NSWindowDelegate {
    private static var open: Set<MessageSourceWindow> = []
    /// Smallest content size the source window can be dragged down to —
    /// enough for a few header lines (same convention as the Outbox window).
    static let minimumContentSize = NSSize(width: 420, height: 240)
    private let window: NSWindow
    private let hosting: NSHostingController<MessageSourceContent>
    /// The local message this window shows. View Source is per message: a
    /// second request for the same message fronts this window (with the
    /// freshly fetched text) instead of stacking a duplicate; other messages
    /// still get their own window.
    let messageId: String

    static func show(text: String, title: String, messageId: String) {
        if let existing = open.first(where: { $0.messageId == messageId }) {
            existing.hosting.rootView = MessageSourceContent(text: text)
            existing.window.title = title
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = MessageSourceWindow(text: text, title: title, messageId: messageId)
        open.insert(controller)
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(text: String, title: String, messageId: String) {
        self.messageId = messageId
        hosting = NSHostingController(rootView: MessageSourceContent(text: text))
        // `.minSize` only: keep the user's size, take the minimum from the view.
        hosting.sizingOptions = [.minSize]
        window = NSWindow(contentViewController: hosting)
        window.title = title
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.setContentSize(NSSize(width: 720, height: 640))
        window.contentMinSize = Self.minimumContentSize
        window.isReleasedWhenClosed = false
        super.init()
        window.delegate = self
    }

    func windowWillClose(_ notification: Notification) {
        Self.open.remove(self)
    }
}
