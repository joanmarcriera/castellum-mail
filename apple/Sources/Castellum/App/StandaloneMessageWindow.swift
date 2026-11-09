// Castellum - a single message opened in its own window (double-click).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Hosts a ReadingPaneView for one message in a standalone AppKit window. Retains
// itself in a static set until the window closes, so it isn't deallocated while
// on screen. Uses the already-fetched detail (shared core, no second DB handle).

import AppKit
import SwiftUI

/// What a standalone window shows: the detail (descriptor-only until its body has been
/// fetched), the hydration state and a retry action. Owned by the window and mutated by
/// `AppModel.beginStandaloneMessage` (#3441).
@MainActor
final class StandaloneMessageState: ObservableObject {
    @Published var detail: MessageDetail
    @Published var isHydrating = false
    @Published var error: String?
    /// Re-runs the body fetch; non-nil only while a Load/Retry button should show.
    @Published var retry: (() -> Void)?
    /// Set when the window closed, so a late fetch result does no further work.
    var isClosed = false
    /// Inline (cid:) image source for the shown detail (#3442).
    var inlineImages: ((MessageDetail) -> InlineImageSource?)?

    init(detail: MessageDetail) { self.detail = detail }
}

private struct StandaloneMessageContent: View {
    @ObservedObject var state: StandaloneMessageState

    var body: some View {
        ReadingPaneView(
            detail: state.detail, isHydratingBody: state.isHydrating,
            bodyHydrationError: state.error, onLoadBody: state.retry,
            inlineImages: state.inlineImages)
    }
}

@MainActor
final class StandaloneMessageWindow: NSObject, NSWindowDelegate {
    private static var open: Set<StandaloneMessageWindow> = []
    private let window: NSWindow
    private let state: StandaloneMessageState

    static func show(state: StandaloneMessageState, title: String) {
        let controller = StandaloneMessageWindow(state: state, title: title)
        open.insert(controller)
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
    }

    static func show(detail: MessageDetail, title: String) {
        show(state: StandaloneMessageState(detail: detail), title: title)
    }

    private init(state: StandaloneMessageState, title: String) {
        self.state = state
        let hosting = NSHostingController(rootView: StandaloneMessageContent(state: state))
        window = NSWindow(contentViewController: hosting)
        window.title = title
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.setContentSize(NSSize(width: 620, height: 640))
        window.isReleasedWhenClosed = false
        super.init()
        window.delegate = self
    }

    func windowWillClose(_ notification: Notification) {
        state.isClosed = true
        Self.open.remove(self)
    }
}
