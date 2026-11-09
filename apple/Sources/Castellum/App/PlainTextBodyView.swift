// Castellum - the plain-text message body with quote and signature folding (MM-17, #2807).
//
// A non-editable, selectable NSTextView in an NSScrollView (SwiftUI `Text` exposes no selection and
// cannot host click targets inside the text). `PlainTextStructure` splits the body into blocks;
// quotes are tinted by depth, and a long quote or signature starts collapsed to one line
// ("▸ 14 quoted lines" / "▸ Signature") that expands on click. The fold lines are chrome: they are
// marked with `.castellumChrome` and are left out of the selection the reader publishes, so Reply
// never quotes them. Bodies over 2 MB, or with folding switched off, render flat.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI

extension NSAttributedString.Key {
    /// Marks a fold line the viewer added (not part of the message text).
    static let castellumChrome = NSAttributedString.Key("castellum.chrome")
}

/// Pure rendering of blocks into an attributed string, so it is testable without a view.
enum PlainTextRendering {
    static let foldScheme = "castellum-fold"

    static func attributedString(
        blocks: [PlainTextStructure.Block], fold: PlainFoldState, foldingEnabled: Bool,
        font: NSFont = .systemFont(ofSize: NSFont.systemFontSize)
    ) -> NSAttributedString {
        let out = NSMutableAttributedString()
        let base: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.labelColor]
        func append(_ lines: [String], attributes: [NSAttributedString.Key: Any], last: Bool) {
            out.append(NSAttributedString(string: lines.joined(separator: "\n") + (last ? "" : "\n"), attributes: attributes))
        }
        for (index, block) in blocks.enumerated() {
            let isLast = index == blocks.count - 1
            let foldable = foldingEnabled && PlainTextStructure.startsFolded(block)
            let folded = foldingEnabled && fold.isFolded(index: index, block: block)
            if folded {
                out.append(chromeLine("▸ " + PlainTextStructure.foldLabel(block), index: index, font: font, last: isLast))
                continue
            }
            if foldable {   // expanded but foldable: offer the way back
                out.append(chromeLine("▾ " + PlainTextStructure.foldLabel(block), index: index, font: font, last: false))
            }
            switch block {
            case .text(let lines):
                append(lines, attributes: base, last: isLast)
            case .quote(let depth, let lines):
                var attrs = base
                attrs[.foregroundColor] = quoteColor(depth: depth)
                attrs[.backgroundColor] = NSColor.controlAccentColor.withAlphaComponent(min(0.04 * CGFloat(depth), 0.14))
                let style = NSMutableParagraphStyle()
                style.firstLineHeadIndent = CGFloat(min(depth, 4)) * 6
                style.headIndent = CGFloat(min(depth, 4)) * 6
                attrs[.paragraphStyle] = style
                append(lines, attributes: attrs, last: isLast)
            case .signature(let lines):
                var attrs = base
                attrs[.foregroundColor] = NSColor.secondaryLabelColor
                append(lines, attributes: attrs, last: isLast)
            }
        }
        return out
    }

    private static func chromeLine(_ text: String, index: Int, font: NSFont, last: Bool) -> NSAttributedString {
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: font.pointSize - 1, weight: .medium),
            .foregroundColor: NSColor.linkColor,
            .link: URL(string: "\(foldScheme):\(index)") as Any,
            .castellumChrome: true,
        ]
        return NSAttributedString(string: text + (last ? "" : "\n"), attributes: attrs)
    }

    private static func quoteColor(depth: Int) -> NSColor {
        let palette: [NSColor] = [.systemBlue, .systemPurple, .systemTeal, .systemOrange]
        return palette[(depth - 1) % palette.count].blended(withFraction: 0.35, of: .labelColor) ?? .secondaryLabelColor
    }

    /// The text of `range` in `string`, without the chrome fold lines.
    static func selectedText(in string: NSAttributedString, range: NSRange) -> String {
        guard range.length > 0, range.location + range.length <= string.length else { return "" }
        var parts: [String] = []
        string.enumerateAttributes(in: range, options: []) { attrs, sub, _ in
            if attrs[.castellumChrome] == nil { parts.append(string.attributedSubstring(from: sub).string) }
        }
        return parts.joined()
    }

    /// The block index a fold link names (`castellum-fold:<n>`), or nil for any other link.
    static func foldIndex(from link: Any) -> Int? {
        guard let url = (link as? URL) ?? (link as? String).flatMap(URL.init(string:)),
              url.scheme == foldScheme else { return nil }
        return Int(url.absoluteString.dropFirst(foldScheme.count + 1))
    }
}

struct PlainTextBodyView: NSViewRepresentable {
    let body: String
    @Binding var fold: PlainFoldState
    let foldingEnabled: Bool
    /// The text currently selected (chrome excluded); empty when nothing is.
    var onSelection: (String) -> Void = { _ in }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSTextView.scrollableTextView()
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        let tv = scroll.documentView as! NSTextView
        tv.isEditable = false
        tv.isSelectable = true
        tv.drawsBackground = false
        tv.textContainerInset = NSSize(width: 0, height: 4)
        tv.isAutomaticLinkDetectionEnabled = false
        tv.linkTextAttributes = [:]   // fold lines carry their own colour
        tv.delegate = context.coordinator
        context.coordinator.textView = tv
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let tv = scroll.documentView as? NSTextView else { return }
        let c = context.coordinator
        c.onToggle = { index in fold.toggle(index: index) }
        c.onSelection = onSelection
        let flat = !foldingEnabled || body.utf8.count > PlainTextStructure.maxStructuredBytes
        let blocks = flat ? [PlainTextStructure.Block.text(lines: [body])] : PlainTextStructure.parse(body)
        let signature = "\(body.hashValue)|\(flat)|\(fold.toggled.sorted())"
        guard c.lastSignature != signature else { return }
        let selection = tv.selectedRange()
        c.lastSignature = signature
        c.applying = true
        tv.textStorage?.setAttributedString(PlainTextRendering.attributedString(
            blocks: blocks, fold: fold, foldingEnabled: !flat))
        if selection.location + selection.length <= (tv.string as NSString).length { tv.setSelectedRange(selection) }
        c.applying = false
    }

    @MainActor
    final class Coordinator: NSObject, NSTextViewDelegate {
        weak var textView: NSTextView?
        var onToggle: (Int) -> Void = { _ in }
        var onSelection: (String) -> Void = { _ in }
        var lastSignature = ""
        var applying = false

        func textView(_ textView: NSTextView, clickedOnLink link: Any, at charIndex: Int) -> Bool {
            guard let index = PlainTextRendering.foldIndex(from: link) else { return false }
            onToggle(index)
            return true
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            guard !applying, let tv = notification.object as? NSTextView, let storage = tv.textStorage else { return }
            onSelection(PlainTextRendering.selectedText(in: storage, range: tv.selectedRange()))
        }
    }
}
