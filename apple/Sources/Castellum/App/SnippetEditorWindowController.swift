// Castellum - Snippets window. Single-reuse window like
// `SignatureEditorWindowController`: opening it again re-fronts it; movable,
// resizable, frame remembered.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI

@MainActor
final class SnippetEditorWindowController: NSObject, NSWindowDelegate {
    static let defaultContentSize = NSSize(width: 700, height: 600)
    static let minimumContentSize = NSSize(width: 600, height: 520)
    static let autosaveName = "CastellumSnippetsWindow"

    private static var shared: SnippetEditorWindowController?
    private let window: NSWindow

    /// True while the window exists (the UIT harness and tests ask).
    static var isOpen: Bool { shared != nil }

    static func show(prefs: Preferences, model: AppModel) {
        if let existing = shared {
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = SnippetEditorWindowController(prefs: prefs, model: model)
        shared = controller
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(prefs: Preferences, model: AppModel) {
        let hosting = NSHostingController(rootView: SnippetEditorView(prefs: prefs, model: model))
        hosting.sizingOptions = [.minSize]
        window = NSWindow(contentViewController: hosting)
        window.title = String(localized: "Snippets")
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.contentMinSize = Self.minimumContentSize
        window.setContentSize(Self.defaultContentSize)
        window.isReleasedWhenClosed = false
        if !window.setFrameUsingName(Self.autosaveName) { window.center() }
        window.setFrameAutosaveName(Self.autosaveName)
        Self.enforceMinimum(on: window)
        super.init()
        window.delegate = self
    }

    /// Declares the minimum content size and grows an undersized window to it.
    static func enforceMinimum(on window: NSWindow) {
        let minimum = minimumContentSize
        if window.contentMinSize != minimum { window.contentMinSize = minimum }
        let current = window.contentRect(forFrameRect: window.frame).size
        if current.width < minimum.width || current.height < minimum.height {
            window.setContentSize(NSSize(
                width: max(current.width, minimum.width), height: max(current.height, minimum.height)))
        }
    }

    func windowDidBecomeKey(_ notification: Notification) { Self.enforceMinimum(on: window) }

    func windowDidUpdate(_ notification: Notification) {
        if window.contentMinSize != Self.minimumContentSize { Self.enforceMinimum(on: window) }
    }

    func windowWillClose(_ notification: Notification) { Self.shared = nil }
}
