// Castellum - Help > Diagnostics window.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Single-reuse window: at most one Diagnostics window exists; opening it again
// re-fronts the existing one rather than stacking duplicates.

import AppKit
import SwiftUI

@MainActor
final class DiagnosticsWindowController: NSObject, NSWindowDelegate {
    static let defaultContentSize = NSSize(width: 700, height: 480)
    static let minimumContentSize = NSSize(width: 580, height: 360)

    /// The one live controller (retains itself while the window is on screen).
    private static var shared: DiagnosticsWindowController?
    private let window: NSWindow

    static func show(model: AppModel) {
        if let existing = shared {
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = DiagnosticsWindowController(model: model)
        shared = controller
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(model: AppModel) {
        let hosting = NSHostingController(rootView: DiagnosticsView(model: model))
        window = NSWindow(contentViewController: hosting)
        window.title = "Diagnostics"
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
