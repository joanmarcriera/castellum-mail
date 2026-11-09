// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 spike (design section 7 step 1): can the app inspect and
// capture its OWN views in-process, with no Accessibility or Screen Recording
// grant? Renders AccountSetupSheet, checks the PNG is not blank, and walks the
// in-process NSAccessibility tree. Outcomes are recorded in DECISIONS.md (R1/R2).
//
// Usage: runs with the normal XCTest target. Set
// `TEST_RUNNER_CASTELLUM_UI_SNAPSHOT_DIR=<dir>` to also keep the PNGs/tree dump.
import AppKit
import SwiftUI
import XCTest
@testable import Castellum

@MainActor
final class UIAxSpikeTests: XCTestCase {
    private func axNodes(_ element: Any, depth: Int = 0, into out: inout [(Int, String, String, NSRect)]) {
        guard depth < 40 else { return }
        guard let obj = element as? NSAccessibilityProtocol else { return }
        out.append((depth, obj.accessibilityRole()?.rawValue ?? "?", obj.accessibilityLabel() ?? "", obj.accessibilityFrame()))
        for k in obj.accessibilityChildren() ?? [] { axNodes(k, depth: depth + 1, into: &out) }
    }

    func testSpikeAccessibilityTreeAndCapture() throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-spike-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        let hosting = NSHostingView(rootView: AccountSetupSheet(model: model))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 420, height: 560),
                              styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.contentView = hosting
        window.layoutIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(0.5))
        hosting.layoutSubtreeIfNeeded()

        // R2: content view and theme frame capture.
        let rep = try XCTUnwrap(hosting.bitmapImageRepForCachingDisplay(in: hosting.bounds))
        hosting.cacheDisplay(in: hosting.bounds, to: rep)
        let frameView = try XCTUnwrap(window.contentView?.superview)
        let frameRep = try XCTUnwrap(frameView.bitmapImageRepForCachingDisplay(in: frameView.bounds))
        frameView.cacheDisplay(in: frameView.bounds, to: frameRep)

        func distinctColours(_ r: NSBitmapImageRep) -> Int {
            var seen = Set<UInt32>()
            guard let d = r.bitmapData else { return 0 }
            let n = r.pixelsWide * r.pixelsHigh
            let spp = r.samplesPerPixel
            for i in stride(from: 0, to: n, by: 7) {
                let p = d + i * spp
                seen.insert(UInt32(p[0]) << 16 | UInt32(p[1]) << 8 | UInt32(p[2]))
                if seen.count > 50 { break }
            }
            return seen.count
        }
        let contentColours = distinctColours(rep), frameColours = distinctColours(frameRep)
        print("SPIKE contentColours=\(contentColours) frameColours=\(frameColours) frame=\(frameView.bounds)")

        // R1: in-process AX tree, no AX client attached.
        var nodes: [(Int, String, String, NSRect)] = []
        axNodes(hosting, into: &nodes)
        let roles = Dictionary(grouping: nodes, by: { $0.1 }).mapValues(\.count)
        print("SPIKE ax nodes=\(nodes.count) roles=\(roles)")
        for n in nodes.prefix(60) { print("SPIKE   \(String(repeating: " ", count: n.0))\(n.1) '\(n.2)' \(n.3)") }

        if let out = ProcessInfo.processInfo.environment["CASTELLUM_UI_SNAPSHOT_DIR"] {
            try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)
            try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: out + "/spike-content.png"))
            try? frameRep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: out + "/spike-frame.png"))
        }
        XCTAssertGreaterThan(contentColours, 3, "content capture is blank")
        // R1 outcome (recorded in DECISIONS.md): the in-process AX tree of an
        // NSHostingView is EMPTY unless an AX client is attached, so discovery
        // must walk NSViews. Assert that walk finds the controls.
        var views: [NSView] = []
        func walk(_ v: NSView) { views.append(v); v.subviews.forEach(walk) }
        walk(hosting)
        let classes = Dictionary(grouping: views, by: { String(describing: type(of: $0)) }).mapValues(\.count)
        print("SPIKE nsviews=\(views.count) classes=\(classes)")
        XCTAssertTrue(views.contains { $0 is NSTextField }, "no NSTextField discoverable")
        XCTAssertTrue(views.contains { $0 is NSButton }, "no NSButton discoverable")
        print("SPIKE axEmptyWithoutClient=\(nodes.count <= 1)")
        withExtendedLifetime(window) {}
    }
}
