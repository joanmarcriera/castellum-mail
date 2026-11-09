// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - shared layout constants and sheet-window policy for Settings
// and the Add / Re-authenticate account sheet (S71/#2647).
//
// Usage: `SettingsLayout` feeds SettingsView's frames; `SheetLayout` feeds
// AccountSetupSheet; `.movableResizableSheet()` applies `SheetWindowPolicy`
// to the sheet's hosting NSWindow so it can be dragged and resized.
import AppKit
import SwiftUI

enum SettingsLayout {
    static let minWidth: CGFloat = 480
    static let idealWidth: CGFloat = 560
    static let maxWidth: CGFloat = 900
    static let minHeight: CGFloat = 340
    static let idealHeight: CGFloat = 480
    /// No ceiling: the user may stretch Settings as tall as the display.
    static let maxHeight: CGFloat = .infinity
    /// Signature TextEditor bound. Small on purpose: it sits under the
    /// account list and must never push the list out of view.
    static let signatureMinHeight: CGFloat = 60
    static let signatureMaxHeight: CGFloat = 100
    static let accountListMinHeight: CGFloat = 120
    /// Cap on the scrolling detail area (signature, Markdown toggle,
    /// identities) so the account list always keeps the rest of the pane.
    static let accountDetailMaxHeight: CGFloat = 200
    /// Width of the right-aligned label column of a `SettingsRow`. Fits the
    /// longest General label ("Clicking an attachment:", 143 pt); longer ones wrap.
    static let labelColumnWidth: CGFloat = 150
    static let labelSpacing: CGFloat = 8
    static let rowSpacing: CGFloat = 8
    static let captionSpacing: CGFloat = 4
    static let sectionSpacing: CGFloat = 12
}

// MARK: - Settings pane rows
//
// Usage:
//   SettingsPane {
//       SettingsSection("Reading") {
//           SettingsRow("Mark messages read:") { Picker(…).labelsHidden(); Text("caption") }
//           SettingsRow { Toggle(…) }          // no label: content column only
//       }
//   }
//
// Why not `Form`: a macOS columns-style Form does not keep its content column
// inside the width it is offered. The column takes the width of its widest
// row, and a row with no label of its own (a caption, a toggle, a row of
// buttons) is offered close to the WHOLE form width before being placed to the
// right of the label column. So the form came out wider than its window at
// every size (527 pt in 480, 953 pt in 900, measured), a vertical ScrollView
// centred the over-wide document, and both edges were clipped with no way to
// scroll to them. Plain stacks are offered exactly the width that is left, so
// text wraps inside the window. Guarded by UILayoutSnapshotTests
// (`testSettingsGeneralFitsItsMinimumWidth`) and the UIT-1 scenarios
// `settings-general-clipping` and `gui3-settings-window`.

/// The vertical stack of sections; takes the width it is offered, never more.
struct SettingsPane<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: SettingsLayout.sectionSpacing) { content }
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A titled group of rows. The title sits in the content column, like a
/// columns Form's section header.
struct SettingsSection<Content: View>: View {
    private let title: LocalizedStringKey
    private let content: Content

    init(_ title: LocalizedStringKey, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: SettingsLayout.rowSpacing) {
            SettingsRow { Text(title).accessibilityAddTraits(.isHeader) }
            content
        }
    }
}

/// One row: a right-aligned label in a fixed-width column, then the content,
/// stacked top to bottom, in whatever width remains. A long label wraps
/// inside its column instead of widening the pane.
struct SettingsRow<Content: View>: View {
    private let label: LocalizedStringKey?
    private let content: Content

    init(_ label: LocalizedStringKey? = nil, @ViewBuilder content: () -> Content) {
        self.label = label
        self.content = content()
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: SettingsLayout.labelSpacing) {
            if let label {
                Text(label)
                    .multilineTextAlignment(.trailing)
                    .frame(width: SettingsLayout.labelColumnWidth, alignment: .trailing)
            } else {
                // Keeps the label column in place so the content still lines up.
                Color.clear.frame(width: SettingsLayout.labelColumnWidth, height: 0)
            }
            VStack(alignment: .leading, spacing: SettingsLayout.captionSpacing) { content }
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

enum SheetLayout {
    static let width: CGFloat = 420
    /// Small enough for a short display; the form scrolls inside.
    static let minHeight: CGFloat = 300
    /// Tall enough to show every Add-account option at once (GUI-1, the tracker
    /// #2684): title, all IMAP/SMTP fields, the First-sync section and the
    /// result banner measure roughly 600 pt since AA1 merged each port and its
    /// TLS toggle into one row; 660 leaves headroom for a result banner and
    /// still opens fully on a 768 pt display (743 visible - 60 margin = 683).
    static let fullContentHeight: CGFloat = 660
    /// Space kept free of the screen's visible frame for the title bar,
    /// menu bar and a margin, so the sheet never opens taller than the display.
    static let screenMargin: CGFloat = 60

    /// The height the sheet opens at: the full form when the display allows,
    /// otherwise as much as fits (the body scrolls, the footer stays pinned).
    static func idealHeight(visibleScreenHeight: CGFloat?) -> CGFloat {
        guard let visibleScreenHeight else { return fullContentHeight }
        return max(minHeight, min(fullContentHeight, visibleScreenHeight - screenMargin))
    }

    static var idealHeight: CGFloat {
        #if DEBUG
        // UI harness: size against the scenario's virtual screen so captures match across machines.
        if let virtual = UIVirtualScreen.override { return idealHeight(visibleScreenHeight: virtual.visibleSize.height) }
        #endif
        return idealHeight(visibleScreenHeight: NSScreen.main?.visibleFrame.height)
    }
}

/// AppKit policy applied to a sheet's window: movable by dragging its
/// background, and resizable with a minimum that keeps the footer usable.
enum SheetWindowPolicy {
    @MainActor
    static func apply(to window: NSWindow) {
        window.isMovable = true
        window.isMovableByWindowBackground = true
        window.styleMask.insert(.resizable)
        window.contentMinSize = minimumContentSize
    }

    static let minimumContentSize = NSSize(width: SheetLayout.width, height: SheetLayout.minHeight)

    /// False once SwiftUI has reconfigured the window and dropped the policy.
    @MainActor
    static func isApplied(to window: NSWindow) -> Bool {
        window.styleMask.contains(.resizable) && window.contentMinSize == minimumContentSize
    }
}

/// Keeps an AppKit window policy applied to a window SwiftUI owns.
///
/// Usage: an NSView placed in the window keeps one of these and calls
/// `watch(window, isApplied:apply:)` from `viewDidMoveToWindow`.
///
/// SwiftUI configures the windows it creates (the `Settings` scene's window, a
/// `.sheet`'s window) AFTER it installs the content view, overwriting a style
/// mask and minimum size set from `viewDidMoveToWindow`. So the policy is
/// applied at once, again on the next run-loop turn, and again whenever the
/// window updates or becomes key and `isApplied` says it has been lost.
/// `didBecomeKey` covers a user opening the window; `didUpdate` covers one
/// that is shown without ever becoming key (scripted UIT-1 runs, background
/// launches). The check is cheap and `apply` is idempotent.
@MainActor
final class WindowPolicyKeeper {
    private var observers: [NSObjectProtocol] = []
    private var styleMaskObservation: NSKeyValueObservation?
    private var minSizeObservation: NSKeyValueObservation?
    private var check: (@MainActor () -> Void)?

    func watch(_ window: NSWindow?,
               isApplied: @escaping @MainActor (NSWindow) -> Bool,
               apply: @escaping @MainActor (NSWindow) -> Void) {
        stop()
        guard let window else { return }
        apply(window)
        DispatchQueue.main.async { [weak window] in
            if let window, !isApplied(window) { apply(window) }
        }
        // queue nil: delivered synchronously on the posting (main) thread, so the
        // policy is back before anything can observe the window without it.
        observers = [NSWindow.didBecomeKeyNotification, NSWindow.didUpdateNotification].map { name in
            NotificationCenter.default.addObserver(forName: name, object: window, queue: nil) { note in
                guard let window = note.object as? NSWindow else { return }
                MainActor.assumeIsolated {
                    if !isApplied(window) { apply(window) }
                }
            }
        }
        // The notifications above only arrive while events flow. SwiftUI also
        // rewrites the style mask when the window is resized programmatically
        // with no event behind it (gui3-settings-window caught the Settings
        // window without .resizable right after a resize), so the mask itself
        // is observed too. Restored on the next main-queue turn, never inside
        // SwiftUI's own configuration pass.
        styleMaskObservation = window.observe(\.styleMask) { window, _ in
            DispatchQueue.main.async { [weak window] in
                if let window, !isApplied(window) { apply(window) }
            }
        }
        // Same for the minimum size: when the Add-account sheet grows an inline
        // error, SwiftUI re-sizes the sheet and resets `contentMinSize` to zero
        // with no style-mask change and no event (u1-gui1-add-sheet-long-error).
        minSizeObservation = window.observe(\.contentMinSize) { window, _ in
            DispatchQueue.main.async { [weak window] in
                if let window, !isApplied(window) { apply(window) }
            }
        }
        check = { [weak window] in
            if let window, !isApplied(window) { apply(window) }
        }
    }

    /// Re-checks the policy now; for hosts that know the window was just
    /// reconfigured (a re-layout of the sheet's content).
    func reassert() { check?() }

    func stop() {
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers = []
        styleMaskObservation = nil
        minSizeObservation = nil
        check = nil
    }

    deinit { observers.forEach { NotificationCenter.default.removeObserver($0) } }
}

/// AppKit policy for the Settings window. SwiftUI's
/// `Settings` scene builds a fixed-size window, so without this the window
/// cannot be resized and forgets where the user put it. Applied once from
/// inside the hosted view, like `SheetWindowPolicy`.
enum SettingsWindowPolicy {
    static let autosaveName = "CastellumSettingsWindow"

    @MainActor
    static func apply(to window: NSWindow) {
        window.isMovable = true
        window.isMovableByWindowBackground = true
        window.styleMask.insert(.resizable)
        window.contentMinSize = NSSize(width: SettingsLayout.minWidth, height: SettingsLayout.minHeight)
        // Restores the saved frame on the next launch and saves on every
        // move/resize. Guarded: re-applying must not reset a live frame.
        if window.frameAutosaveName != autosaveName {
            window.setFrameAutosaveName(autosaveName)
        }
    }

    /// False once SwiftUI has reconfigured the window and dropped the policy.
    /// Only the style mask is checked: the Settings scene keeps adjusting the
    /// minimum size itself (it adds the tab toolbar's height), and competing
    /// with it on every window update would be a tug of war.
    @MainActor
    static func isApplied(to window: NSWindow) -> Bool {
        window.styleMask.contains(.resizable)
    }
}

private struct SettingsWindowConfigurator: NSViewRepresentable {
    /// Applies `SettingsWindowPolicy` and keeps it applied (see `WindowPolicyKeeper`).
    final class Probe: NSView {
        private let keeper = WindowPolicyKeeper()

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            keeper.watch(window, isApplied: SettingsWindowPolicy.isApplied(to:), apply: SettingsWindowPolicy.apply(to:))
        }
    }
    func makeNSView(context: Context) -> Probe { Probe() }
    func updateNSView(_ nsView: Probe, context: Context) {}
}

/// Full-size background NSView behind the sheet's content. It applies the
/// window policy once attached, keeps it applied, and acts as a drag handle: AppKit ignores
/// `isMovableByWindowBackground` on an attached sheet (verified by
/// SettingsSheetSnapshotTests), so a click on empty sheet background starts
/// `performDrag` explicitly. Controls above it keep receiving their own clicks.
final class SheetDragBackgroundView: NSView {
    override var mouseDownCanMoveWindow: Bool { true }
    override func mouseDown(with event: NSEvent) {
        window?.performDrag(with: event)
    }
    private let keeper = WindowPolicyKeeper()

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        // Kept, not just applied: SwiftUI reconfigures a `.sheet`'s window after
        // attaching the content and would drop the minimum size (GUI-1).
        keeper.watch(window, isApplied: SheetWindowPolicy.isApplied(to:), apply: SheetWindowPolicy.apply(to:))
    }

    /// The sheet's content re-laid out (an inline error appeared, the sheet grew):
    /// SwiftUI may have rewritten the window's minimum size on the way, so the
    /// policy is checked again on the next turn, outside SwiftUI's own pass.
    override func layout() {
        super.layout()
        DispatchQueue.main.async { [weak self] in self?.keeper.reassert() }
    }
}

private struct SheetWindowConfigurator: NSViewRepresentable {
    func makeNSView(context: Context) -> SheetDragBackgroundView { SheetDragBackgroundView() }
    func updateNSView(_ nsView: SheetDragBackgroundView, context: Context) {}
}

extension View {
    /// Make the enclosing Settings window movable, resizable and remembered.
    func settingsWindowPolicy() -> some View {
        background(SettingsWindowConfigurator().frame(width: 0, height: 0))
    }

    /// Make the enclosing sheet window draggable and resizable.
    func movableResizableSheet() -> some View {
        background(SheetWindowConfigurator())
    }
}
