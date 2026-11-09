// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 view-tree inspection (Debug builds only).
//
// Usage: `UIViewTree.info(for: window, label:)` and `UIViewTree.nodes(in: window)`
// turn a live NSWindow into the value types `UIInvariants` evaluates, and
// `find` resolves a `UIFind` to a live control for press/setText/toggle.
//
// Discovery for layout rules walks NSViews, NOT the accessibility tree: an
// NSHostingView's in-process AX tree is empty until the app is told a client
// wants it (DECISIONS.md, UIT-1 R1). Up to macOS 26 SwiftUI's bordered Button/
// Toggle/Picker were AppKit-backed (NSButton/NSPopUpButton subclasses) and so
// discoverable here; on macOS 27 only TextField/TextEditor still are, and the
// rest is reached through `UIAXDriver`. Plain SwiftUI `Text`
// is drawn, not a view: it is only visible in the PNG evidence.
#if DEBUG
import AppKit

@MainActor
enum UIViewTree {
    struct Entry {
        var node: UINode
        weak var view: NSView?
        /// A SwiftUI accessibility node (macOS 27 controls with no NSControl); see `UIAXDriver`.
        var ax: AnyObject?
    }

    static func info(for window: NSWindow, label: String) -> UIWindowInfo {
        let content = window.contentView
        let contentRect = content.map { $0.convert($0.bounds, to: nil) } ?? .zero
        let frame = window.frame
        let contentSize = window.contentRect(forFrameRect: frame).size
        return UIWindowInfo(
            label: label, title: window.title, cls: String(describing: type(of: window)),
            frame: frame, contentRect: contentRect, contentLayoutRect: window.contentLayoutRect,
            contentMinSize: window.contentMinSize,
            chrome: CGSize(width: frame.width - contentSize.width, height: frame.height - contentSize.height),
            resizable: window.styleMask.contains(.resizable), movable: window.isMovable,
            isSheet: window.sheetParent != nil,
            hasDragHandle: containsDragHandle(content),
            realScreenVisible: window.screen?.visibleFrame, nonBlankRatio: nil)
    }

    private static func containsDragHandle(_ v: NSView?) -> Bool {
        guard let v else { return false }
        if v is SheetDragBackgroundView { return true }
        return v.subviews.contains { containsDragHandle($0) }
    }

    static func entries(in window: NSWindow) -> [Entry] {
        guard let root = window.contentView else { return [] }
        var out: [Entry] = []
        walk(root, path: "", inScroll: false, into: &out)
        return out
    }

    static func nodes(in window: NSWindow) -> [UINode] { entries(in: window).map(\.node) }

    private static func walk(_ v: NSView, path: String, inScroll: Bool, into out: inout [Entry]) {
        let scroll = inScroll || v is NSScrollView || v is NSClipView
        if let p = v as? UIProbeView {
            out.append(Entry(node: UINode(
                path: path.isEmpty ? "/" : path, cls: "UIProbe", role: p.role, label: p.label, identifier: p.probeId,
                frame: p.convert(p.bounds, to: nil), hidden: p.isHiddenOrHasHiddenAncestor, enabled: p.probeEnabled,
                keyEquivalent: p.keyEquivalent, inScrollView: inScroll, textOverflow: false), view: p))
        }
        // A vertical-only scroll view whose document is wider than its viewport clips content
        // sideways with no way to scroll to it (the #2646 shape for SwiftUI Text, which has no
        // NSView of its own). Report the document's horizontal extent as a pseudo-node.
        if let sv = v as? NSScrollView, !sv.hasHorizontalScroller, let doc = sv.documentView {
            let d = doc.convert(doc.bounds, to: nil), c = sv.contentView.convert(sv.contentView.bounds, to: nil)
            out.append(Entry(node: UINode(
                path: path.isEmpty ? "/" : path, cls: String(describing: type(of: sv)), role: "scrollContent",
                label: "scroll content", identifier: "", frame: CGRect(x: d.minX, y: c.minY, width: d.width, height: c.height),
                hidden: sv.isHiddenOrHasHiddenAncestor, enabled: true, keyEquivalent: "", inScrollView: inScroll,
                textOverflow: false), view: nil))
        }
        if let ctl = v as? NSControl {
            out.append(Entry(node: node(for: ctl, path: path, inScroll: inScroll), view: ctl))
        } else if String(describing: type(of: v)) == "KeyViewProxy" {
            // macOS 27: SwiftUI no longer backs Picker/Toggle/Button with NSControl subclasses; it
            // keeps one private `KeyViewProxy` per focusable control, framed exactly like the
            // control. Report it so layout rules still see every control. Frame only, and that
            // frame goes stale after a window resize until the key-view loop is recalculated:
            // Layer B does not act on these, it resolves the control through `UIAXDriver`.
            out.append(Entry(node: UINode(
                path: path.isEmpty ? "/" : path, cls: "KeyViewProxy", role: "control",
                label: v.accessibilityLabel() ?? "", identifier: "", frame: v.convert(v.bounds, to: nil),
                hidden: v.isHiddenOrHasHiddenAncestor, enabled: true, keyEquivalent: "",
                inScrollView: inScroll, textOverflow: false), view: nil))
        }
        for (i, s) in v.subviews.enumerated() {
            walk(s, path: path + "/" + String(String(describing: type(of: s)).prefix(40)) + "[\(i)]", inScroll: scroll, into: &out)
        }
    }

    private static func node(for c: NSControl, path: String, inScroll: Bool) -> UINode {
        var role = "control"
        var label = c.accessibilityLabel() ?? ""
        var overflow = false
        var keyEq = ""
        if let p = c as? NSPopUpButton {
            role = "popup"; if label.isEmpty { label = p.titleOfSelectedItem ?? "" }
        } else if let b = c as? NSButton {
            switch b.accessibilityRole() {
            case .checkBox?: role = "checkbox"
            case .radioButton?: role = "radio"
            default: role = "button"
            }
            if label.isEmpty { label = b.title }
            keyEq = b.keyEquivalent
        } else if let t = c as? NSTextField {
            if t.isEditable {
                role = String(describing: type(of: t)).contains("Secure") ? "secureTextField" : "textField"
                if label.isEmpty { label = t.placeholderString ?? "" }
            } else {
                role = "staticText"
                if label.isEmpty { label = t.stringValue }
                let big = NSRect(x: 0, y: 0, width: 100_000, height: max(c.bounds.height, 1))
                let natural = t.cell?.cellSize(forBounds: big).width ?? 0
                let truncates = t.lineBreakMode == .byTruncatingTail || t.lineBreakMode == .byTruncatingHead
                    || t.lineBreakMode == .byTruncatingMiddle
                overflow = truncates && natural > c.bounds.width + 1
            }
        }
        return UINode(
            path: path.isEmpty ? "/" : path, cls: String(describing: type(of: c)), role: role, label: label,
            identifier: c.accessibilityIdentifier(), frame: c.convert(c.bounds, to: nil),
            hidden: c.isHiddenOrHasHiddenAncestor, enabled: c.isEnabled, keyEquivalent: keyEq,
            inScrollView: inScroll, textOverflow: overflow)
    }

    /// True when any part of `v` lies outside an enclosing scroll clip view's visible bounds or
    /// outside the window's content view, i.e. the user would have to scroll (or resize) to see it all.
    static func clippedOut(_ v: NSView) -> Bool {
        let slack: CGFloat = 0.5
        var cur = v.superview
        while let c = cur {
            if c is NSClipView || c === v.window?.contentView {
                let r = v.convert(v.bounds, to: c)
                if !c.bounds.insetBy(dx: -slack, dy: -slack).contains(r) { return true }
            }
            cur = c.superview
        }
        return false
    }

    static func matches(_ f: UIFind, _ n: UINode) -> Bool {
        if let id = f.identifier, n.identifier != id { return false }
        if let l = f.label, n.label != l { return false }
        if let r = f.role, n.role != r { return false }
        if let t = f.text, !n.label.contains(t) { return false }
        return f.identifier != nil || f.label != nil || f.role != nil || f.text != nil
    }

    /// NSView / probe matches only. Verbs use `resolve` / `resolveVisible` (UIAXDriver.swift),
    /// which add SwiftUI's accessibility nodes when no view matches.
    static func find(_ f: UIFind, in window: NSWindow) -> [Entry] {
        entries(in: window).filter { matches(f, $0.node) }
    }

    /// JSON dump used as evidence (`tree.json`).
    static func json(window: UIWindowInfo, nodes: [UINode]) -> Data {
        struct Dump: Encodable { var window: UIWindowInfo; var nodes: [UINode] }
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        return (try? enc.encode(Dump(window: window, nodes: nodes))) ?? Data()
    }
}
#endif
