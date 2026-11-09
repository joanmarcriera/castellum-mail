// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 Layer A helpers: host a real SwiftUI view in an offscreen
// window (or as a real attached sheet), capture it, and run the SAME
// invariants the Layer B runner uses (`UIRule`, `UIViewTree`, `UICapture`).
//
// Usage: `let r = UISnapshotHarness.hostSheet(view)`; `r.assertRules([...])`.
// When `CASTELLUM_UI_SNAPSHOT_DIR` is set (pass
// `TEST_RUNNER_CASTELLUM_UI_SNAPSHOT_DIR=<dir>` to xcodebuild, as
// `scripts/ui-check.sh --unit` does) PNGs and tree.json land in
// `<dir>/<TestName>/`; the assertions run either way. Uses a temp profile and the
// in-memory Keychain (TestHost), never the real profile.
import AppKit
import SwiftUI
import XCTest
@testable import Castellum

@MainActor
struct UISnapshotResult {
    let window: NSWindow
    let parent: NSWindow?
    let info: UIWindowInfo
    let nodes: [UINode]

    /// Evaluates `rules` and fails the calling test with one XCTFail per violation.
    func assertRules(_ rules: [String], screen: UIVirtualScreen = .standard,
                     file: StaticString = #filePath, line: UInt = #line) {
        for rule in rules {
            for v in UIRule.evaluate(rule, window: info, nodes: nodes, screen: screen) {
                XCTFail("[\(rule)] \(v.element): \(v.detail)", file: file, line: line)
            }
        }
    }
}

/// A window that ignores frame autosave. `SettingsWindowPolicy` gives any window hosting
/// `SettingsView` the autosave name "CastellumSettingsWindow", so a plain `NSWindow` restored
/// whatever frame the Debug app's defaults held (480x340 after a minimum-size test) instead of
/// the size the test asked for, and saved its own frame back. Harness windows neither read nor
/// write that default.
final class UIHarnessWindow: NSWindow {
    override func setFrameAutosaveName(_ name: NSWindow.FrameAutosaveName) -> Bool { false }
    override func setFrameUsingName(_ name: NSWindow.FrameAutosaveName, force: Bool) -> Bool { false }
    override func saveFrame(usingName name: NSWindow.FrameAutosaveName) {}
}

@MainActor
enum UISnapshotHarness {
    static func makeModel() -> AppModel {
        AppModel(profileDirectory: TestScratch.root
            .appendingPathComponent("castellum-uit-\(UUID().uuidString)", isDirectory: true))
    }

    static func settle(_ window: NSWindow, _ seconds: TimeInterval = 0.5) {
        window.layoutIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(seconds))
        window.contentView?.layoutSubtreeIfNeeded()
    }

    /// A plain titled, resizable window (like Settings) with `view` as content.
    static func hostWindow<V: View>(_ view: V, size: NSSize, name: String,
                                    testName: String = #function) -> UISnapshotResult {
        let hosting = NSHostingView(rootView: view)
        let w = UIHarnessWindow(contentRect: NSRect(origin: .zero, size: size),
                                styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        w.contentView = hosting
        w.setContentSize(size)
        settle(w)
        return snapshot(w, parent: nil, name: name, testName: testName)
    }

    /// `view` presented in a REAL attached sheet of a 900x700 parent window.
    static func hostSheet<V: View>(_ view: V, name: String, testName: String = #function) -> UISnapshotResult {
        let parent = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 700),
                              styleMask: [.titled], backing: .buffered, defer: false)
        let sheet = NSWindow(contentViewController: NSHostingController(rootView: view))
        parent.beginSheet(sheet)
        settle(sheet)
        return snapshot(sheet, parent: parent, name: name, testName: testName)
    }

    /// Like `hostWindow`, but also snapshots any WKWebView (reader-pane body) via
    /// `UICapture.webSnapshots` and composites it into the PNG; innerText is in `UICapture.lastWebText`.
    static func hostWindowWithWeb<V: View>(_ view: V, size: NSSize, name: String,
                                           testName: String = #function) async -> UISnapshotResult {
        let hosting = NSHostingView(rootView: view)
        let w = UIHarnessWindow(contentRect: NSRect(origin: .zero, size: size),
                                styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        w.contentView = hosting
        w.setContentSize(size)
        w.orderFront(nil)
        settle(w)
        let web = await UICapture.webSnapshots(in: w)
        return snapshot(w, parent: nil, name: name, testName: testName, web: web)
    }

    static func snapshot(_ w: NSWindow, parent: NSWindow?, name: String, testName: String,
                         web: [UICapture.WebSnapshot] = []) -> UISnapshotResult {
        var info = UIViewTree.info(for: w, label: name)
        let dir = ProcessInfo.processInfo.environment["CASTELLUM_UI_SNAPSHOT_DIR"].map {
            URL(fileURLWithPath: $0).appendingPathComponent(String(testName.prefix { $0 != "(" }), isDirectory: true)
        }
        // Always render (the nonBlank rule needs it); only keep files when a dir was given.
        let out = dir ?? TestScratch.root.appendingPathComponent("castellum-uit-png-\(UUID().uuidString)")
        let res = UICapture.capture(window: w, label: name, dir: out, web: web)
        info.nonBlankRatio = res.nonBlankRatio
        let nodes = UIViewTree.nodes(in: w)
        if dir != nil {
            let data = UIViewTree.json(window: info, nodes: nodes)
            try? data.write(to: out.appendingPathComponent("\(UICapture.sanitise(name)).tree.json"))
        } else {
            try? FileManager.default.removeItem(at: out)
        }
        return UISnapshotResult(window: w, parent: parent, info: info, nodes: nodes)
    }
}
