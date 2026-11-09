// Castellum - app-lifetime routes for menu/chord commands that need no
// main-window state (FP44).
//
// Every other command route is an `.onReceive` on RootView, which SwiftUI
// tears down with the main WindowGroup window: after ⌘W on the main window
// (while e.g. Status or Contacts stays open) those menu items silently did
// nothing, and nothing reopened the main window. The routes below only call
// AppModel `open…()` methods, so they live here instead, installed ONCE by
// AppDelegate.modelBecameAvailable and never removed; RootView no longer
// observes them, so no command fires twice while the main window is open.
// Each still runs through CommandDispatcher (same focus policy as RootView).
//
// "Main Window" (Window menu) fronts the main window, or reopens it through
// the SwiftUI OpenWindowAction captured when RootView first appeared (the
// action is app-scoped; it outlives the view that handed it over).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI

@MainActor
final class AppCommandRouter {
    /// The main WindowGroup's scene id (CastellumApp).
    static let mainWindowId = "main"

    private var observers: [NSObjectProtocol] = []
    /// The scene's OpenWindowAction, captured when the first main window
    /// appeared (see `Installer`).
    private let openWindow: () -> OpenWindowAction?

    init(model: AppModel, openWindow: @escaping () -> OpenWindowAction?) {
        self.openWindow = openWindow
        let dispatch = CommandDispatcher()
        let routes: [(Notification.Name, () -> Void)] = [
            (.castellumShowContacts, { [weak model] in model?.openContacts() }),
            (.castellumShowProjects, { [weak model] in model?.openProjectsBoard() }),
            (.castellumShowDiagnostics, { [weak model] in model?.openDiagnostics() }),
            (.castellumShowOutbox, { [weak model] in model?.openOutbox() }),
            (.castellumSendAllOutbox, { [weak model] in Task { _ = await model?.sendAllOutboxEverywhere() } }),
            (.castellumShowStatus, { [weak model] in model?.openStatusDashboard() }),
            (.castellumShowAiActivity, { [weak model] in model?.openAiActivity() }),
            (.castellumCompose, { [weak model] in
                guard let model,
                      let accountId = model.composeAccountIdOrPromptUnavailable() else { return }
                ComposeWindowController.show(
                    model: model,
                    session: ComposeSession(accountId: accountId,
                                            signature: Preferences.shared.signature(for: accountId)),
                    aiCoordinator: model.aiCoordinator)
            }),
            (.castellumShowMainWindow, { [weak self] in self?.showMainWindow() }),
        ]
        for (name, action) in routes {
            // Posted on the main thread by menus, chords and the UI harness.
            observers.append(NotificationCenter.default.addObserver(
                forName: name, object: nil, queue: .main
            ) { _ in
                MainActor.assumeIsolated { dispatch.perform(name, action: action) }
            })
        }
    }

    /// Front the main window if one exists, else open a new one.
    func showMainWindow() {
        NSApp.activate(ignoringOtherApps: true)
        // A closed window can linger in NSApp.windows: only front a live one.
        if let main = NSApp.windows.first(where: {
            Self.isMainWindow($0) && ($0.isVisible || $0.isMiniaturized)
        }) {
            main.makeKeyAndOrderFront(nil)
        } else {
            openWindow()?(id: Self.mainWindowId)
        }
    }

    /// Hands the scene's OpenWindowAction to the router (see the header).
    /// Stored on the AppDelegate, not the router: this onAppear can run
    /// before RootView's (which creates the router).
    struct Installer: View {
        let store: (OpenWindowAction) -> Void
        @Environment(\.openWindow) private var openWindow

        var body: some View {
            Color.clear.onAppear { store(openWindow) }
        }
    }

    /// SwiftUI names WindowGroup windows "<scene id>-AppWindow-<n>"; the
    /// scene title is the fallback (the UI harness labels "main" the same way).
    static func isMainWindow(_ window: NSWindow) -> Bool {
        (window.identifier?.rawValue ?? "").hasPrefix("\(mainWindowId)-")
            || (window.title == "Castellum" && !(window is NSPanel))
    }
}
