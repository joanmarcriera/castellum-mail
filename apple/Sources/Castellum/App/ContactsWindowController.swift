// Castellum - the Contacts window (g c / ⌘⇧C).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Same self-retaining pattern as StandaloneMessageWindow, but SINGLE-reuse:
// there is at most one Contacts window; opening it again re-fronts the
// existing one instead of stacking duplicates.
//
// `show(model:graphTab:projectId:)` opens the window, forces the Graph tab
// when requested, and passes the projectId filter through to ContactsView.

import AppKit
import SwiftUI

@MainActor
final class ContactsWindowController: NSObject, NSWindowDelegate {
    /// The one live controller (retains itself while the window is on screen).
    private static var shared: ContactsWindowController?
    private let window: NSWindow
    /// Mutable so the caller can change the project filter after the window
    /// is already open (e.g. when "Show network" is called a second time).
    private let projectIdBox: ObservableBox<String?>

    // MARK: - Public entry points

    /// Open (or bring to front) the Contacts window, optionally pre-navigated
    /// to the Graph tab filtered by `projectId`.
    static func show(
        model: AppModel,
        graphTab: Bool = false,
        projectId: String? = nil
    ) {
        if let existing = shared {
            if graphTab {
                // ContactsWindowRoot observes the box, so this publishes a
                // re-render; ContactsView's onChange(of: filterProjectId)
                // then switches to the Graph tab.
                existing.projectIdBox.value = projectId
            }
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        let box = ObservableBox<String?>(projectId)
        let controller = ContactsWindowController(model: model, projectIdBox: box)
        shared = controller
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    // MARK: - Init

    private init(model: AppModel, projectIdBox: ObservableBox<String?>) {
        self.projectIdBox = projectIdBox
        let hosting = NSHostingController(
            rootView: ContactsWindowRoot(box: projectIdBox, model: model)
        )
        window = NSWindow(contentViewController: hosting)
        window.title = "Contacts"
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.setContentSize(NSSize(width: 760, height: 520))
        window.isReleasedWhenClosed = false
        super.init()
        window.delegate = self
    }

    func windowWillClose(_ notification: Notification) {
        Self.shared = nil
    }
}

// MARK: - Root wrapper + ObservableBox

/// Root view that OBSERVES the box: without this, mutating `box.value` from
/// outside the view hierarchy would never trigger a re-render, and
/// ContactsView's `onChange(of: filterProjectId)` would never fire.
private struct ContactsWindowRoot: View {
    @ObservedObject var box: ObservableBox<String?>
    let model: AppModel

    var body: some View {
        ContactsView(model: model, filterProjectId: $box.value)
    }
}

/// A reference-type box so we can update the SwiftUI binding from outside the
/// view hierarchy without recreating the window.
private final class ObservableBox<T>: ObservableObject {
    @Published var value: T
    init(_ value: T) { self.value = value }
}
