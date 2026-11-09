// Castellum - keep the main window on its screen.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The main window is created by SwiftUI's `WindowGroup`, which sets no
// min/max size and never clamps the frame to the screen. SwiftUI resizes
// that window PROGRAMMATICALLY when the layout's required width changes
// (NavigationSplitView sidebar show/hide, the reading-pane toggle) — and
// AppKit only screen-constrains USER-driven resizes, not `setFrame`. So a
// zoomed, full-screen-width window grows past the screen edge the moment a
// collapsed column comes back. The fix: watch `didResize` and clamp the
// frame back inside the current screen's `visibleFrame`.

import AppKit
import SwiftUI

// MARK: - Pure clamp math (unit-tested in WindowClampTests)

/// Returns `current` adjusted so it fits entirely inside `visible`:
/// - wider/taller than `visible` → shrunk to fit;
/// - hanging past any edge → moved back inside (top edge included, so the
///   title bar can never end up under the menu bar or off-screen).
/// A frame already inside `visible` is returned unchanged.
/// A degenerate `visible` (zero/negative width or height — e.g. a screen in
/// a transient reconfiguration state) returns `current` unchanged: shrinking
/// into a zero rect would make the window invisible, and `contentMinSize`
/// does NOT constrain programmatic `setFrame`.
func clampedWindowFrame(current: NSRect, visible: NSRect) -> NSRect {
    guard visible.width > 0, visible.height > 0 else { return current }
    var frame = current
    frame.size.width = min(frame.width, visible.width)
    frame.size.height = min(frame.height, visible.height)
    if frame.maxX > visible.maxX { frame.origin.x = visible.maxX - frame.width }
    if frame.minX < visible.minX { frame.origin.x = visible.minX }
    if frame.maxY > visible.maxY { frame.origin.y = visible.maxY - frame.height }
    if frame.minY < visible.minY { frame.origin.y = visible.minY }
    return frame
}

// MARK: - Fight guard (pure, unit-tested in WindowClampTests)

/// Detects the clamp fighting SwiftUI's own window sizing.
///
/// When a split-view column is dragged, SwiftUI grows the window from inside
/// its layout pass (`NSHostingView.windowDidLayout` → `setFrame`). If that
/// frame is wider than the screen and the clamp shrinks it back synchronously
/// from `didResize`, SwiftUI's next pass grows it again, and AppKit throws
/// `NSGenericException` "more Update Constraints in Window passes than there
/// are views in the window" — the 0.3.3 crash on resizing the sidebar
/// (macOS 26.6). The guard remembers the size it just clamped; the same size
/// arriving again before the run loop turns is the fight, and the clamp
/// yields (the window overflows the screen edge for that drag, which is
/// cosmetic). `endTurn()` runs on the next main-queue turn so a later zoom
/// to the same size is clamped normally.
struct ClampFightGuard {
    private var clampedThisTurn: NSSize?

    /// Returns `true` when `size` was already clamped in this run-loop turn.
    /// Otherwise records it and returns `false` (clamp it).
    mutating func shouldYield(to size: NSSize) -> Bool {
        if clampedThisTurn == size { return true }
        clampedThisTurn = size
        return false
    }

    mutating func endTurn() {
        clampedThisTurn = nil
    }
}

// MARK: - Clamper

/// Owns the clamp for one window: sets a minimum content size and, after
/// every resize (zoom, or SwiftUI's layout-driven `setFrame` when a split
/// column toggles), snaps the frame back inside the CURRENT screen's
/// `visibleFrame`. Deliberately not a window delegate — SwiftUI installs
/// its own delegate on WindowGroup windows and replacing it would break
/// scene behavior. Resize-only (no move observation) so dragging between
/// displays is never fought; `window.screen` tracks the display the window
/// is mostly on. Full-screen windows are exempt (their frame legitimately
/// covers the whole screen, not just `visibleFrame`).
@MainActor
final class WindowScreenClamper: NSObject {
    /// Below the full three-pane minimum on purpose (about 1237 pt in
    /// widescreen: the sidebar counted twice, as its column and as the
    /// detail column's inset, plus list 420 + reader 360; see RootView): with
    /// the sidebar collapsed or the reading pane hidden the layout needs
    /// less, and SwiftUI already enforces the per-pane minimums. This only
    /// stops the window shrinking into uselessness.
    static let minimumContentSize = NSSize(width: 480, height: 360)

    private weak var window: NSWindow?
    /// Re-entrancy guard: our own `setFrame` re-posts `didResize`, and SwiftUI
    /// ANIMATED resizes post it at every animation step — clamping from inside
    /// those callbacks would abort the animation mid-flight (visible snap).
    /// Everything here is main-thread-synchronous, so a plain Bool suffices.
    private var isAdjusting = false
    private var fightGuard = ClampFightGuard()
    private var turnResetScheduled = false

    init(window: NSWindow) {
        self.window = window
        super.init()
        window.contentMinSize = Self.minimumContentSize
        NotificationCenter.default.addObserver(
            self, selector: #selector(windowDidResize(_:)),
            name: NSWindow.didResizeNotification, object: window)
        clamp()  // a restored-oversized frame is fixed on first appearance
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
    }

    /// NSWindow notifications post on the main thread.
    @objc private func windowDidResize(_ note: Notification) {
        MainActor.assumeIsolated { clamp() }
    }

    private func clamp() {
        guard !isAdjusting,
              let window,
              !window.styleMask.contains(.fullScreen),
              let screen = window.screen ?? NSScreen.main
        else { return }
        let visible = screen.visibleFrame
        // Degenerate geometry (e.g. mid display-reconfiguration): never
        // setFrame toward a zero rect — the window would become invisible.
        guard visible.width > 0, visible.height > 0 else { return }
        let clamped = clampedWindowFrame(current: window.frame, visible: visible)
        guard clamped != window.frame else { return }
        // SwiftUI re-growing the window to the size we just clamped, inside
        // the same run-loop turn, is the layout fight described on
        // `ClampFightGuard`: yield rather than crash.
        if fightGuard.shouldYield(to: window.frame.size) { return }
        if !turnResetScheduled {
            turnResetScheduled = true
            DispatchQueue.main.async { [weak self] in
                self?.fightGuard.endTurn()
                self?.turnResetScheduled = false
            }
        }
        isAdjusting = true
        window.setFrame(clamped, display: true)
        isAdjusting = false
    }
}

// MARK: - SwiftUI hook

/// Zero-size background view that installs a `WindowScreenClamper` on the
/// hosting window once SwiftUI attaches it (the standard trick for reaching
/// the `WindowGroup`'s NSWindow). Apply as `.background(WindowScreenClampAccessor())`.
struct WindowScreenClampAccessor: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView { InstallerView() }
    func updateNSView(_ nsView: NSView, context: Context) {}

    private final class InstallerView: NSView {
        private var clamper: WindowScreenClamper?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window else {
                // Detached from the window: tear down so the clamper's
                // deinit removes its observer; a later re-attach installs a
                // fresh clamper bound to the new window.
                clamper = nil
                return
            }
            guard clamper == nil else { return }
            clamper = WindowScreenClamper(window: window)
        }
    }
}
