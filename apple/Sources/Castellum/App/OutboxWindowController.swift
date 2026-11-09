// Castellum - Outbox window (undelivered operations + manual recovery).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Single-reuse window: at most one Outbox window exists; opening it again
// re-fronts the existing one rather than stacking duplicates.

import AppKit
import SwiftUI

@MainActor
final class OutboxWindowController: NSObject, NSWindowDelegate {
    static let defaultContentSize = NSSize(width: 560, height: 400)
    static let minimumContentSize = NSSize(width: 520, height: 320)

    /// The one live controller (retains itself while the window is on screen).
    private static var shared: OutboxWindowController?
    private let window: NSWindow

    static func show(model: AppModel) {
        if let existing = shared {
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = OutboxWindowController(model: model)
        shared = controller
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(model: AppModel) {
        let hosting = NSHostingController(rootView: OutboxView(model: model))
        window = NSWindow(contentViewController: hosting)
        window.title = "Outbox"
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.contentMinSize = Self.minimumContentSize
        window.setContentSize(Self.defaultContentSize)
        window.isReleasedWhenClosed = false
        super.init()
        window.delegate = self
    }

    func windowWillClose(_ notification: Notification) {
        Self.shared = nil
    }
}
