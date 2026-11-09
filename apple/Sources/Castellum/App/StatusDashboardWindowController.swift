// Castellum - Window > Status dashboard.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Single-reuse window: at most one Status window exists; opening it again
// re-fronts the existing one rather than stacking duplicates.

import AppKit
import SwiftUI

@MainActor
final class StatusDashboardWindowController: NSObject, NSWindowDelegate {
    static let defaultContentSize = NSSize(width: 620, height: 460)
    static let minimumContentSize = NSSize(width: 520, height: 340)

    /// The one live controller (retains itself while the window is on screen).
    private static var shared: StatusDashboardWindowController?
    private let window: NSWindow

    static func show(model: AppModel) {
        if let existing = shared {
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = StatusDashboardWindowController(model: model)
        shared = controller
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(model: AppModel) {
        let hosting = NSHostingController(rootView: StatusDashboardView(model: model))
        window = NSWindow(contentViewController: hosting)
        window.title = "Status"
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
