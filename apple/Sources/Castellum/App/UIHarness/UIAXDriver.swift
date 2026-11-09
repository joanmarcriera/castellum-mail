// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 Layer B driver for SwiftUI controls that have no NSControl (Debug builds only).
//
// Usage: nothing calls this directly. `UIViewTree.resolve` falls back to `UIAXDriver.entries(in:)`
// when the NSView walk finds no visible match, and the runner's verbs (press, toggle, setPopup,
// popupSelect, pressExpect, expectPopupItems, waitNode, `expect find`) then act on the entry's
// accessibility node through `press(_:)` / `menu(of:choose:)`.
//
// Why (, project-docs/uit1-layer-b-macos27-2026-10-12.md): on macOS 27 a SwiftUI
// Button / Toggle / Picker is no longer an NSButton / NSPopUpButton. What remains in the NSView
// tree is a private `KeyViewProxy` with no label and a frame that goes stale after a resize.
// SwiftUI still builds an accessibility tree of `SwiftUI.AccessibilityNode` objects (role, label,
// identifier, value, frame, enabled), but only once the app is told a client wants it. Setting
// `AXEnhancedUserInterface` on NSApp FROM INSIDE the process does that: no AX client, no
// Accessibility or Screen Recording grant, no mouse. Measured facts this file depends on:
//   - `accessibilityPerformPress()` on a node runs a Button's action and flips a Toggle.
//   - A Picker ignores setAccessibilityValue / increment / decrement. Its press opens the real
//     NSMenu (a modal tracking loop); the item is chosen from a common-modes run-loop block the
//     moment tracking begins, and tracking is cancelled, so nothing waits for a mouse.
//   - Nodes are not `NSAccessibilityProtocol` to Swift: call them through `AnyObject` dispatch.
//   - Never query an `NSAccessibilityRemoteUIElement` (WKWebView content): the call abandons the
//     running Swift job and later crashes in `swift_task_isMainExecutor`. The walk skips them.
//   - Actions run from a plain run-loop block (how a real AX request arrives), not from a job.
#if DEBUG
import AppKit
import Foundation

@MainActor
enum UIAXDriver {
    private(set) static var isEnabled = false
    private static var readyWindows = Set<Int>()

    /// Asks SwiftUI to build its accessibility tree (idempotent, in-process only).
    static func enable() {
        guard !isEnabled else { return }
        isEnabled = true
        NSApp.accessibilitySetValue(true, forAttribute: NSAccessibility.Attribute(rawValue: "AXEnhancedUserInterface"))
    }

    /// Tests only: back to the state of a process that never asked for the tree.
    static func reset() {
        guard isEnabled else { return }
        isEnabled = false
        readyWindows = []
        NSApp.accessibilitySetValue(false, forAttribute: NSAccessibility.Attribute(rawValue: "AXEnhancedUserInterface"))
    }

    /// Enables the tree and waits (once per window, at most 1.5 s) until SwiftUI has populated it.
    static func ready(_ w: NSWindow) async {
        enable()
        guard !readyWindows.contains(w.windowNumber) else { return }
        let deadline = Date().addingTimeInterval(1.5)
        while entries(in: w).isEmpty && Date() < deadline { try? await Task.sleep(nanoseconds: 50_000_000) }
        readyWindows.insert(w.windowNumber)
    }

    /// SwiftUI accessibility nodes of a window as tree entries (`view` is nil, `ax` is the node).
    static func entries(in w: NSWindow) -> [UIViewTree.Entry] {
        enable()
        guard let root = w.contentView else { return [] }
        var out: [UIViewTree.Entry] = []
        walk(root, window: w, depth: 0, into: &out)
        return out
    }

    private static func walk(_ o: AnyObject, window w: NSWindow, depth: Int, into out: inout [UIViewTree.Entry]) {
        guard depth < 80, out.count < 2000 else { return }
        let cls = String(describing: type(of: o))
        // Remote (out-of-process) elements and web content: see the header. Never touched.
        if cls.contains("RemoteUIElement") || cls.contains("WKWebView") { return }
        if cls == "AccessibilityNode", let n = node(for: o, window: w) { out.append(UIViewTree.Entry(node: n, view: nil, ax: o)) }
        for k in o.accessibilityChildren?() ?? [] { walk(k as AnyObject, window: w, depth: depth + 1, into: &out) }
    }

    private static let roles: [String: String] = [
        "AXButton": "button", "AXCheckBox": "checkbox", "AXPopUpButton": "popup", "AXMenuButton": "menuButton",
        "AXStaticText": "staticText", "AXTextField": "textField", "AXRadioButton": "radio", "AXImage": "image",
        "AXLink": "link", "AXSlider": "slider",
    ]

    private static func value(of o: AnyObject) -> String {
        guard let n = o as? NSObject, n.responds(to: NSSelectorFromString("accessibilityValue")),
              let v = n.value(forKey: "accessibilityValue") else { return "" }
        return (v as? String) ?? (v as? NSNumber)?.stringValue ?? ""
    }

    private static func node(for o: AnyObject, window w: NSWindow) -> UINode? {
        guard let raw = o.accessibilityRole?()?.rawValue, let role = roles[raw] else { return nil }   // groups etc. are containers
        var label = o.accessibilityLabel?() ?? ""
        // Same convention as the NSView walk: a pop-up or a text is known by what it shows.
        if label.isEmpty || role == "popup" { let v = value(of: o); if !v.isEmpty { label = v } }
        let screen = o.accessibilityFrame?() ?? .zero
        return UINode(
            path: "ax", cls: "AccessibilityNode", role: role, label: label,
            identifier: o.accessibilityIdentifier?() ?? "", frame: w.convertFromScreen(screen),
            hidden: false, enabled: o.isAccessibilityEnabled?() ?? true, keyEquivalent: "",
            inScrollView: false, textOverflow: false)
    }

    /// "1"/"0" state of a checkbox node (nil when it reports neither).
    static func isOn(_ ax: AnyObject) -> Bool? {
        switch value(of: ax) { case "1": return true; case "0": return false; default: return nil }
    }

    /// Runs `body` from a plain run-loop block on the main thread, outside any Swift job.
    private static func onRunLoop(_ body: @escaping @MainActor () -> Bool) async -> Bool {
        await withCheckedContinuation { (c: CheckedContinuation<Bool, Never>) in
            CFRunLoopPerformBlock(CFRunLoopGetMain(), CFRunLoopMode.commonModes.rawValue) {
                c.resume(returning: MainActor.assumeIsolated { body() })
            }
            CFRunLoopWakeUp(CFRunLoopGetMain())
        }
    }

    /// The accessibility press: a Button's action, a Toggle's flip.
    static func press(_ e: UIViewTree.Entry) async throws {
        guard let ax = e.ax else { throw UIStepError(message: "control is not pressable") }
        guard e.node.enabled else { throw UIStepError(message: "control '\(e.node.label)' is disabled") }
        guard await onRunLoop({ ax.accessibilityPerformPress?() ?? false }) else {
            throw UIStepError(message: "control '\(e.node.label)' refused the accessibility press")
        }
    }

    private final class MenuVisit: @unchecked Sendable {   // main thread only
        var titles: [String]?
        var done = false
    }

    /// Opens a Picker's menu, returns its item titles, and (when `title` is given) chooses that
    /// item exactly as a click on it would. The menu never waits for a mouse: see the header.
    @discardableResult
    static func menu(of e: UIViewTree.Entry, choose title: String?) async throws -> [String] {
        guard let ax = e.ax else { throw UIStepError(message: "control is not a pop-up") }
        guard e.node.enabled else { throw UIStepError(message: "pop-up '\(e.node.label)' is disabled") }
        let visit = MenuVisit()
        let obs = NotificationCenter.default.addObserver(forName: NSMenu.didBeginTrackingNotification, object: nil, queue: nil) { note in
            MainActor.assumeIsolated {
                guard visit.titles == nil, let m = note.object as? NSMenu else { return }
                visit.titles = m.items.map(\.title)
                CFRunLoopPerformBlock(CFRunLoopGetMain(), CFRunLoopMode.commonModes.rawValue) {
                    MainActor.assumeIsolated {
                        let i = title.flatMap { t in m.items.firstIndex { $0.title == t } }
                        m.cancelTrackingWithoutAnimation()
                        if let i, m.items[i].isEnabled { m.performActionForItem(at: i) }
                        visit.done = true
                    }
                }
                CFRunLoopWakeUp(CFRunLoopGetMain())
            }
        }
        defer { NotificationCenter.default.removeObserver(obs) }
        _ = await onRunLoop { ax.accessibilityPerformPress?() ?? false }
        let deadline = Date().addingTimeInterval(3)
        while !visit.done && Date() < deadline { try? await Task.sleep(nanoseconds: 30_000_000) }
        guard visit.done, let titles = visit.titles else { throw UIStepError(message: "pop-up '\(e.node.label)' opened no menu") }
        if let title, !titles.contains(title) { throw UIStepError(message: "pop-up has no item '\(title)'; items: \(titles)") }
        try? await Task.sleep(nanoseconds: 100_000_000)   // let SwiftUI apply the selection
        return titles
    }
}

extension UIViewTree.Entry {
    /// Presses the control the way its kind allows: probe action, AppKit click, accessibility press.
    @MainActor func press() async throws {
        if let p = view as? UIProbeView {
            guard p.probeEnabled else { throw UIStepError(message: "control '\(p.label)' is disabled") }
            guard let a = p.action else { throw UIStepError(message: "probe '\(p.probeId)' has no action") }
            a()
        } else if let c = view as? NSControl {
            guard c.isEnabled else { throw UIStepError(message: "control is disabled") }
            c.performClick(nil)
        } else { try await UIAXDriver.press(self) }
    }
}

extension UIViewTree {
    /// NSView / probe matches; when none of them is visible, SwiftUI accessibility matches too.
    /// Call `await UIAXDriver.ready(window)` first when the window may not have been walked yet.
    static func resolve(_ f: UIFind, in window: NSWindow) -> [Entry] {
        let views = find(f, in: window)
        if views.contains(where: { !$0.node.hidden }) { return views }
        return views + UIAXDriver.entries(in: window).filter { matches(f, $0.node) }
    }

    /// First visible match, waiting up to `timeoutMs` for the accessibility tree to catch up.
    static func resolveVisible(_ f: UIFind, in window: NSWindow, timeoutMs: Int = 1000) async -> Entry? {
        /// `f.index`-th visible match, top of the window first (window coordinates have y up).
        func pick(_ hits: [Entry]) -> Entry? {
            let visible = hits.filter { !$0.node.hidden }
            guard let i = f.index else { return visible.first }
            let ordered = visible.sorted { ($0.node.frame.maxY, -$0.node.frame.minX) > ($1.node.frame.maxY, -$1.node.frame.minX) }
            return i >= 0 && i < ordered.count ? ordered[i] : nil
        }
        if let e = pick(find(f, in: window)) { return e }
        await UIAXDriver.ready(window)
        let deadline = Date().addingTimeInterval(Double(timeoutMs) / 1000)
        while true {
            if let e = pick(resolve(f, in: window)) { return e }
            if Date() > deadline { return nil }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
    }

    /// Pop-ups of a window, top first: AppKit ones, else SwiftUI's accessibility ones (macOS 27).
    static func popupTitles(in window: NSWindow) -> [String] {
        let appkit = entries(in: window).filter { $0.view is NSPopUpButton && !$0.node.hidden }
        let all = appkit.isEmpty ? UIAXDriver.entries(in: window).filter { $0.node.role == "popup" } : appkit
        return all.sorted { $0.node.frame.maxY > $1.node.frame.maxY }
            .map { ($0.view as? NSPopUpButton)?.titleOfSelectedItem ?? $0.node.label }
    }
}
#endif
