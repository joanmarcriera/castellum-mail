// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 layout invariants (Debug builds only).
//
// Usage: `UIRule.evaluate(rule, window:, nodes:, screen:)` is PURE: it reads only
// the value types below (built from AppKit by UIViewTree), so it is unit-tested
// with synthetic nodes (UIInvariantsTests) and reused by Layer A snapshot tests
// and the Layer B runner. Coordinates are window base coordinates (origin
// bottom-left, y up), the same space for `UIWindowInfo.contentRect` and node frames.
#if DEBUG
import CoreGraphics
import Foundation

/// One inspectable control in a window's NSView tree.
struct UINode: Codable, Equatable {
    var path: String
    var cls: String
    var role: String            // button | textField | secureTextField | staticText | popup | checkbox | control
    var label: String
    var identifier: String
    var frame: CGRect
    var hidden: Bool            // hidden itself or via an ancestor
    var enabled: Bool
    var keyEquivalent: String
    var inScrollView: Bool
    /// Non-editable NSTextField only: the cell's natural width exceeds its bounds and it truncates.
    var textOverflow: Bool
}

struct UIWindowInfo: Codable, Equatable {
    var label: String
    var title: String
    var cls: String
    var frame: CGRect                // screen coordinates
    var contentRect: CGRect          // window base coordinates
    var contentLayoutRect: CGRect
    var contentMinSize: CGSize
    var chrome: CGSize               // frame size minus content size (title bar etc.)
    var resizable: Bool
    var movable: Bool
    var isSheet: Bool
    var hasDragHandle: Bool
    var realScreenVisible: CGRect?   // nil when the window is on no screen
    var nonBlankRatio: Double?       // filled by UICapture; nil = not captured
}

struct UIVirtualScreen: Codable, Equatable {
    var width: Double
    var height: Double
    var menuBar: Double
    static let standard = UIVirtualScreen(width: 1280, height: 768, menuBar: 25)
    var visibleSize: CGSize { CGSize(width: width, height: height - menuBar) }

    /// Debug env override `CASTELLUM_UI_VIRTUAL_SCREEN=WxH` (D4): layout code that
    /// would otherwise read `NSScreen.main` may ask `UIVirtualScreen.override`
    /// so captures do not differ per machine.
    static var override: UIVirtualScreen? = {
        guard let v = ProcessInfo.processInfo.environment["CASTELLUM_UI_VIRTUAL_SCREEN"] else { return nil }
        let p = v.lowercased().split(separator: "x").compactMap { Double($0) }
        return p.count == 2 ? UIVirtualScreen(width: p[0], height: p[1], menuBar: 25) : nil
    }()
}

struct UIViolation: Codable, Equatable {
    var rule: String
    var window: String
    var element: String
    var detail: String
}

enum UIRule {
    static let allNames: Set<String> = [
        "nonBlank", "resizable", "movable", "movableSheet", "fitsScreen", "onScreen",
        "primaryButtonsVisible", "controlsReachable", "noZeroSizeTextFields", "noClippedText",
        "noErrorAlert",
    ]

    /// Rules whose verdict comes from walking the view tree.
    static let nodeRules: Set<String> = ["primaryButtonsVisible", "controlsReachable", "noZeroSizeTextFields", "noClippedText"]

    static func evaluate(_ rule: String, window w: UIWindowInfo, nodes: [UINode],
                         screen: UIVirtualScreen) -> [UIViolation] {
        func v(_ element: String, _ detail: String) -> UIViolation {
            UIViolation(rule: rule, window: w.label, element: element, detail: detail)
        }
        let tol: CGFloat = 1.0
        // A node rule that inspected nothing proved nothing: report it instead of passing.
        if nodeRules.contains(rule) && nodes.isEmpty { return [v("-", "0 nodes inspected; the rule proved nothing")] }
        switch rule {
        case "nonBlank":
            guard let r = w.nonBlankRatio else { return [v("-", "window was not captured")] }
            return r >= 0.005 ? [] : [v("-", String(format: "only %.2f%% of pixels differ from the background", r * 100))]
        case "resizable":
            return w.resizable ? [] : [v("-", "styleMask lacks .resizable")]
        case "movable":
            return w.movable ? [] : [v("-", "isMovable is false")]
        case "movableSheet":
            var out: [UIViolation] = []
            if !w.movable { out.append(v("-", "sheet isMovable is false")) }
            if !w.hasDragHandle { out.append(v("-", "no SheetDragBackgroundView in the view tree")) }
            return out
        case "fitsScreen":
            let vis = screen.visibleSize
            var out: [UIViolation] = []
            if w.frame.width > vis.width + tol || w.frame.height > vis.height + tol {
                out.append(v("-", "frame \(fmt(w.frame.size)) exceeds virtual screen \(fmt(vis))"))
            }
            let minW = w.contentMinSize.width + w.chrome.width, minH = w.contentMinSize.height + w.chrome.height
            if minW > vis.width + tol || minH > vis.height + tol {
                out.append(v("-", "minimum size \(fmt(CGSize(width: minW, height: minH))) exceeds virtual screen \(fmt(vis))"))
            }
            return out
        case "onScreen":
            guard let s = w.realScreenVisible else { return [v("-", "window is on no screen")] }
            let inflated = s.insetBy(dx: -tol, dy: -tol)
            return inflated.contains(w.frame) ? [] : [v("-", "frame \(fmt(w.frame)) not inside screen \(fmt(s))")]
        case "primaryButtonsVisible":
            var out: [UIViolation] = []
            if !nodes.contains(where: { $0.role == "button" }) { return [v("-", "0 buttons inspected; the rule proved nothing")] }
            for n in nodes where n.role == "button" {
                let isDefault = n.keyEquivalent == "\r" || n.keyEquivalent == "\u{1b}"
                guard isDefault || !n.inScrollView else { continue }
                if n.hidden { out.append(v(n.path, "button '\(n.label)' is hidden")); continue }
                if n.frame.width < 1 || n.frame.height < 1 { out.append(v(n.path, "button '\(n.label)' has zero size")); continue }
                if !w.contentLayoutRect.insetBy(dx: -tol, dy: -tol).contains(n.frame) && !n.inScrollView {
                    out.append(v(n.path, "button '\(n.label)' frame \(fmt(n.frame)) outside content \(fmt(w.contentLayoutRect))"))
                }
            }
            return out
        case "controlsReachable":
            var out: [UIViolation] = []
            for n in nodes where n.enabled && !n.hidden && !n.inScrollView && n.frame.width >= 1 && n.role != "scrollContent" {
                if !w.contentLayoutRect.insetBy(dx: -tol, dy: -tol).contains(n.frame) {
                    out.append(v(n.path, "'\(n.label)' (\(n.role)) frame \(fmt(n.frame)) is outside the window and not in a scroll view"))
                }
            }
            return out
        case "noZeroSizeTextFields":
            return nodes.filter { ($0.role == "textField" || $0.role == "secureTextField") && !$0.hidden
                && ($0.frame.width < 20 || $0.frame.height < 12) }
                .map { v($0.path, "text field \(fmt($0.frame.size)) is too small to use") }
        case "noClippedText":
            var out: [UIViolation] = []
            for n in nodes where !n.hidden {
                if n.textOverflow { out.append(v(n.path, "text '\(n.label)' is wider than its box and truncated")) }
                if !n.inScrollView, n.frame.width >= 1,
                   !w.contentRect.insetBy(dx: -tol, dy: -tol).contains(n.frame) {
                    out.append(v(n.path, "'\(n.label)' (\(n.role)) crosses the window edge: \(fmt(n.frame)) vs \(fmt(w.contentRect))"))
                }
            }
            return out
        case "noErrorAlert":
            return []   // needs model + AppKit state; evaluated by the runner
        default:
            return [v("-", "unknown rule")]
        }
    }

    private static func fmt(_ r: CGRect) -> String { "\(Int(r.minX)),\(Int(r.minY)) \(fmt(r.size))" }
    private static func fmt(_ s: CGSize) -> String { "\(Int(s.width))x\(Int(s.height))" }
}
#endif
