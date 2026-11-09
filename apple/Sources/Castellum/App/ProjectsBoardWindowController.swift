// Castellum - Projects board window (⇧⌘P / g p).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Single-reuse window: at most one Projects window exists; opening it again
// re-fronts the existing one instead of stacking duplicates.

import AppKit
import SwiftUI

@MainActor
final class ProjectsBoardWindowController: NSObject, NSWindowDelegate {
    static let defaultContentSize = NSSize(width: 980, height: 640)
    static let minimumContentSize = NSSize(width: 820, height: 540)

    /// The one live controller (retains itself while the window is on screen).
    private static var shared: ProjectsBoardWindowController?
    private let window: NSWindow

    static func show(model: AppModel) {
        if let existing = shared {
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let controller = ProjectsBoardWindowController(model: model)
        shared = controller
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    private init(model: AppModel) {
        let hosting = NSHostingController(rootView: ProjectsBoardView(model: model))
        window = NSWindow(contentViewController: hosting)
        window.title = "Projects"
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
