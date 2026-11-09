// Castellum - Signatures window.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Single-reuse window: at most one exists; opening it again re-fronts it
// (optionally selecting an account's default signature). Movable, resizable,
// frame remembered — unlike the sheet/inline editor it replaces.

import AppKit
import SwiftUI

/// Lets "Signatures…" on another account re-target the already-open window.
/// `token` changes on every request so asking for the same account twice
/// still fires.
@MainActor
final class SignatureWindowRequest: ObservableObject {
    @Published private(set) var accountId: String?
    @Published private(set) var token = UUID()

    func request(accountId: String?) {
        self.accountId = accountId
        token = UUID()
    }
}

@MainActor
final class SignatureEditorWindowController: NSObject, NSWindowDelegate {
    static let request = SignatureWindowRequest()

    static let defaultContentSize = NSSize(width: 720, height: 480)
    static let minimumContentSize = NSSize(width: 600, height: 400)
    static let autosaveName = "CastellumSignaturesWindow"

    private static var shared: SignatureEditorWindowController?
    private let window: NSWindow

    static func show(prefs: Preferences, model: AppModel, accountId: String? = nil) {
        if let existing = shared {
            request.request(accountId: accountId)
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = SignatureEditorWindowController(prefs: prefs, model: model, accountId: accountId)
        shared = controller
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(prefs: Preferences, model: AppModel, accountId: String?) {
        let hosting = NSHostingController(rootView: SignatureEditorView(prefs: prefs, model: model, initialAccountId: accountId))
        // Keep the user's (or autosaved) window size; don't snap to content.
        // `.minSize` only: with `[]` the hosting controller zeroed the
        // window's contentMinSize on first layout (review pack D1); the view's
        // own minimum frame (minimumContentSize) now feeds it, and `enforceMinimum` backs it.
        hosting.sizingOptions = [.minSize]
        window = NSWindow(contentViewController: hosting)
        window.title = "Signatures"
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.contentMinSize = Self.minimumContentSize
        window.setContentSize(Self.defaultContentSize)
        window.isReleasedWhenClosed = false
        // No saved frame yet -> centre; otherwise the autosave restores it.
        if !window.setFrameUsingName(Self.autosaveName) { window.center() }
        window.setFrameAutosaveName(Self.autosaveName)
        // After the frame restore: a saved frame smaller than the minimum
        // comes back at the minimum, and the minimum is (re)asserted last so
        // nothing above (hosting controller, restore) can leave it at 0x0.
        Self.enforceMinimum(on: window)
        super.init()
        window.delegate = self
    }

    /// Declares the minimum content size and grows an undersized window
    /// (e.g. a smaller saved frame) to it. Static so XCTest can drive it on a
    /// bare NSWindow.
    static func enforceMinimum(on window: NSWindow) {
        let minimum = minimumContentSize
        if window.contentMinSize != minimum { window.contentMinSize = minimum }
        let current = window.contentRect(forFrameRect: window.frame).size
        if current.width < minimum.width || current.height < minimum.height {
            window.setContentSize(NSSize(
                width: max(current.width, minimum.width),
                height: max(current.height, minimum.height)))
        }
    }

    /// Re-assert on every activation: cheap, and keeps the harness/user from
    /// ever seeing a 0x0 minimum.
    func windowDidBecomeKey(_ notification: Notification) {
        Self.enforceMinimum(on: window)
    }

    /// The hosting controller can reset `contentMinSize` to zero while it lays
    /// the SwiftUI view out (the harness saw 0x0 after the first layout), so
    /// re-assert it on every window update; a no-op when already correct.
    func windowDidUpdate(_ notification: Notification) {
        if window.contentMinSize != Self.minimumContentSize { Self.enforceMinimum(on: window) }
    }

    /// Belt and braces for user drags: never resize below the minimum even if
    /// `contentMinSize` was reset a moment ago.
    func windowWillResize(_ sender: NSWindow, to frameSize: NSSize) -> NSSize {
        let minimum = Self.minimumContentSize
        let content = sender.contentRect(forFrameRect: NSRect(origin: .zero, size: frameSize)).size
        guard content.width < minimum.width || content.height < minimum.height else { return frameSize }
        return sender.frameRect(forContentRect: NSRect(origin: .zero, size: NSSize(
            width: max(content.width, minimum.width), height: max(content.height, minimum.height)))).size
    }

    func windowWillClose(_ notification: Notification) {
        Self.shared = nil
    }
}
