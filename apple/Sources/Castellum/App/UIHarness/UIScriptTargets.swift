// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 window discovery, labelling and openers (Debug builds only).
//
// Usage: the runner resolves a window LABEL to a live NSWindow with
// `UIWindows.resolve`. Labels: `main`; provenance labels (a window that first
// appears after `openWindow settings` is `settings`, after `openSheet addAccount`
// it is `sheet:addAccount`); `title:<t>`; `class:<NSWindow subclass>`;
// `sheetOf:<label>` (whatever sheet is attached to that window).
// Openers call the real entry points (AppModel.open*, notifications), never
// synthetic clicks.
#if DEBUG
import AppKit

@MainActor
enum UIWindows {
    /// Real, user-visible windows: not tooltips/menus/popover chrome, non-empty.
    static func candidates() -> [NSWindow] {
        NSApp.windows.filter { w in
            let cls = String(describing: type(of: w))
            return w.isVisible && w.contentView != nil && w.frame.width > 8 && w.frame.height > 8
                && !cls.hasPrefix("_NSPopover") && !cls.contains("Tooltip") && !cls.hasPrefix("NSStatusBar")
                && !cls.contains("MenuWindow") && !cls.contains("NSToolbarFullScreen")
        }
    }
}

@MainActor
final class UIWindowLabels {
    private var byLabel: [String: Int] = [:]      // label -> windowNumber
    private var known: Set<Int> = []
    private(set) var pending: [String] = []

    /// Call once before any step: labels the launch window `main`, and marks
    /// everything present as already known.
    func baseline() {
        let wins = UIWindows.candidates()
        if let main = wins.first(where: { $0.title == "Castellum" }) ?? wins.first {
            byLabel["main"] = main.windowNumber
        }
        known = Set(wins.map(\.windowNumber))
    }

    /// The next new window that appears is given `label` (sheets when it starts
    /// with `sheet:`; ordinary windows otherwise).
    func expect(_ label: String) { pending.append(label) }

    func refresh() {
        for w in UIWindows.candidates() where !known.contains(w.windowNumber) {
            let isSheet = w.sheetParent != nil
            if let i = pending.firstIndex(where: { $0.hasPrefix("sheet:") == isSheet }) {
                byLabel[pending.remove(at: i)] = w.windowNumber
            }
            known.insert(w.windowNumber)
        }
    }

    func resolve(_ label: String) -> NSWindow? {
        refresh()
        let wins = UIWindows.candidates()
        if label.hasPrefix("title:") { return wins.first { $0.title == String(label.dropFirst(6)) } }
        if label.hasPrefix("class:") { return wins.first { String(describing: type(of: $0)) == String(label.dropFirst(6)) } }
        // `sheetOf:<label>` = the sheet attached to that labelled window (WS1: an NSAlert raised by the
        // app itself, not by a harness step, so there is no opener to give it a provenance label).
        if label.hasPrefix("sheetOf:") { return resolve(String(label.dropFirst(8)))?.attachedSheet }
        if let n = byLabel[label] { return wins.first { $0.windowNumber == n } }
        // No fallback: `sheet:<x>` resolves only to a sheet labelled by provenance, so a wrong
        // target can never silently bind to whatever sheet happens to be attached.
        return nil
    }

    /// Every (label, window) pair currently known, plus unlabelled visible windows.
    func labelled() -> [(String, NSWindow)] {
        refresh()
        var out: [(String, NSWindow)] = []
        let wins = UIWindows.candidates()
        var used = Set<Int>()
        for (l, n) in byLabel.sorted(by: { $0.key < $1.key }) {
            if let w = wins.first(where: { $0.windowNumber == n }) { out.append((l, w)); used.insert(n) }
        }
        for w in wins where !used.contains(w.windowNumber) {
            // An unlabelled sheet on a labelled window is named after its parent, so a capture can ask for it.
            if let parent = w.sheetParent, let l = out.first(where: { $0.1 === parent })?.0 {
                out.append(("sheetOf:\(l)", w))
                continue
            }
            out.append(("window\(w.windowNumber)-\(String(describing: type(of: w)))", w))
        }
        return out
    }
}
#endif
