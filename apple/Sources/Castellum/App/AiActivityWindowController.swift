// Castellum - Window > AI Activity.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Single-reuse window: at most one AI Activity window exists; opening it
// again re-fronts the existing one rather than stacking duplicates (mirrors
// StatusDashboardWindowController).

import AppKit
import SwiftUI

@MainActor
final class AiActivityWindowController: NSObject, NSWindowDelegate {
    static let defaultContentSize = NSSize(width: 560, height: 420)
    static let minimumContentSize = NSSize(width: 460, height: 300)

    /// The one live controller (retains itself while the window is on screen).
    private static var shared: AiActivityWindowController?
    private let window: NSWindow

    static func show(model: AppModel) {
        if let existing = shared {
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = AiActivityWindowController(model: model)
        shared = controller
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(model: AppModel) {
        let hosting = NSHostingController(
            rootView: AiActivityView(model: model, coordinator: model.aiCoordinator)
        )
        window = NSWindow(contentViewController: hosting)
        window.title = "AI Activity"
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
