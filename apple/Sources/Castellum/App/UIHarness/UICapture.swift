// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 in-process window capture (Debug builds only).
//
// Usage: `UICapture.capture(window:label:dir:)` renders the window's content view
// (`<label>.png`) and its theme frame incl. title bar (`<label>.frame.png`) with
// `cacheDisplay(in:to:)`. That draws the view hierarchy into a bitmap in this
// process: no WindowServer capture, so no Screen Recording grant. Blind spots:
// NSVisualEffectView materials render flat and WKWebView content is blank
// (out-of-process). `webSnapshots(in:)` closes that gap: it asks each WKWebView for its
// own `takeSnapshot` (async, in-process API, no grant), reads `document.body.innerText`,
// and `capture(..., web:)` composites the images over the blank rectangles and writes
// `<label>.body.txt`. The UIT-1 spike (DECISIONS.md) confirmed the frame capture
// includes the traffic lights and title bar.
#if DEBUG
import AppKit
import WebKit

@MainActor
enum UICapture {
    struct Result {
        var contentPath: String?
        var framePath: String?
        var nonBlankRatio: Double
    }

    /// One WKWebView's pixels and text, taken ahead of the (sync) window render.
    struct WebSnapshot {
        weak var view: WKWebView?
        var image: NSImage
        var text: String
    }

    /// innerText of every web view captured by the last `webSnapshots` call (read by the
    /// `reading:webBodyHas:<text>` model expectation).
    static var lastWebText: [String] = []

    /// Snapshots every WKWebView under `window`'s content view once it has finished loading
    /// and has body text (or `timeout` passes; a blank result is then reported, not hidden).
    static func webSnapshots(in window: NSWindow, timeout: TimeInterval = 15) async -> [WebSnapshot] {
        var views: [WKWebView] = []
        func walk(_ v: NSView) { if let w = v as? WKWebView { views.append(w) }; v.subviews.forEach(walk) }
        if let root = window.contentView { walk(root) }
        var out: [WebSnapshot] = []
        for web in views where web.bounds.width >= 1 && web.bounds.height >= 1 {
            var text = ""
            let deadline = Date().addingTimeInterval(timeout)
            repeat {
                if !web.isLoading {
                    text = (try? await web.evaluateJavaScript("document.body ? document.body.innerText : ''") as? String) ?? ""
                    if !text.isEmpty { break }
                }
                try? await Task.sleep(nanoseconds: 100_000_000)
            } while Date() < deadline
            let cfg = WKSnapshotConfiguration()
            let image: NSImage? = await withCheckedContinuation { c in
                web.takeSnapshot(with: cfg) { img, _ in c.resume(returning: img) }
            }
            if let image { out.append(WebSnapshot(view: web, image: image, text: text)) }
        }
        lastWebText = out.map(\.text)
        return out
    }

    static func capture(window: NSWindow, label: String, dir: URL, web: [WebSnapshot] = []) -> Result {
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        window.layoutIfNeeded()
        let safe = sanitise(label)
        var result = Result(contentPath: nil, framePath: nil, nonBlankRatio: 0)
        if !web.isEmpty {
            let body = web.map(\.text).joined(separator: "\n---\n")
            try? body.write(to: dir.appendingPathComponent("\(safe).body.txt"), atomically: true, encoding: .utf8)
        }
        if let view = window.contentView, let rep = render(view) {
            overlay(web, on: rep, in: view)
            result.nonBlankRatio = nonBlankRatio(rep)
            let url = dir.appendingPathComponent("\(safe).png")
            if write(rep, to: url) { result.contentPath = url.path }
        }
        if let frameView = window.contentView?.superview, let rep = render(frameView) {
            overlay(web, on: rep, in: frameView)
            let url = dir.appendingPathComponent("\(safe).frame.png")
            if write(rep, to: url) { result.framePath = url.path }
            // Some hosting setups (the Settings TabView) render nothing through the content
            // view alone but draw fine through the theme frame. Judge the frame capture
            // below the title bar/toolbar instead of reporting a false blank.
            if result.nonBlankRatio < 0.005 {
                let chrome = max(0, frameView.bounds.height - window.contentLayoutRect.maxY)
                let scale = Double(rep.pixelsHigh) / Double(max(frameView.bounds.height, 1))
                result.nonBlankRatio = max(result.nonBlankRatio,
                                           nonBlankRatio(rep, skipTopRows: Int(chrome * scale)))
            }
        }
        return result
    }

    static func sanitise(_ label: String) -> String {
        String(label.map { $0.isLetter || $0.isNumber || $0 == "-" || $0 == "_" ? $0 : "_" })
    }

    private static func render(_ view: NSView) -> NSBitmapImageRep? {
        guard view.bounds.width >= 1, view.bounds.height >= 1,
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return nil }
        view.cacheDisplay(in: view.bounds, to: rep)
        return rep
    }

    /// Draws each web snapshot over its (blank) rectangle in the already-rendered `rep` of `view`.
    private static func overlay(_ web: [WebSnapshot], on rep: NSBitmapImageRep, in view: NSView) {
        guard !web.isEmpty, let ctx = NSGraphicsContext(bitmapImageRep: rep) else { return }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        for w in web {
            guard let v = w.view, v.window === view.window else { continue }
            var r = view.convert(v.bounds, from: v)
            if view.isFlipped { r.origin.y = view.bounds.height - r.maxY }   // bitmap context is bottom-up
            w.image.draw(in: r, from: .zero, operation: .sourceOver, fraction: 1)
        }
        NSGraphicsContext.restoreGraphicsState()
    }

    private static func write(_ rep: NSBitmapImageRep, to url: URL) -> Bool {
        guard let png = rep.representation(using: .png, properties: [:]) else { return false }
        return (try? png.write(to: url)) != nil
    }

    /// Fraction of pixels whose (quantised) colour differs from the most common one.
    /// A blank or failed render is ~0; a real form is well above the 0.5% threshold.
    static func nonBlankRatio(_ rep: NSBitmapImageRep, skipTopRows: Int = 0) -> Double {
        guard let data = rep.bitmapData, rep.pixelsWide > 0, rep.pixelsHigh > 0 else { return 0 }
        let spp = max(rep.samplesPerPixel, 1)
        var counts: [UInt32: Int] = [:]
        var total = 0
        for y in min(skipTopRows, rep.pixelsHigh - 1)..<rep.pixelsHigh {
            let row = data + y * rep.bytesPerRow
            for x in 0..<rep.pixelsWide {
                let p = row + x * spp
                let key = spp >= 3
                    ? UInt32(p[0] >> 3) << 16 | UInt32(p[1] >> 3) << 8 | UInt32(p[2] >> 3)
                    : UInt32(p[0] >> 3)
                counts[key, default: 0] += 1
                total += 1
            }
        }
        guard total > 0, let modal = counts.values.max() else { return 0 }
        return Double(total - modal) / Double(total)
    }
}
#endif
