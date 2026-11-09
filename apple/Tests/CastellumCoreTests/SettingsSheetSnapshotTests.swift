// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S71: renders the REAL Settings and
// Add-account views into an offscreen window at short sizes.
//
// Usage: when `S71_SHOT_DIR` is set (pass `TEST_RUNNER_S71_SHOT_DIR=<dir>` to
// xcodebuild) the PNGs are written there; they back the before/after images in
// project-docs/screenshots/S71/. The assertions hold with or without it.
// Uses a temp profile dir and the in-memory Keychain (TestHost), never the
// real profile.
import AppKit
import SwiftUI
import XCTest
@testable import Castellum

@MainActor
final class SettingsSheetSnapshotTests: XCTestCase {
    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-s71-\(UUID().uuidString)")
        return AppModel(profileDirectory: dir)
    }

    /// Host `view` in an offscreen window of `size`, lay out, return both.
    /// `UIHarnessWindow`: a plain window hosting `SettingsView` restores and saves the
    /// "CastellumSettingsWindow" frame in the Debug app's defaults, so `size` was not guaranteed.
    private func host<V: View>(_ view: V, size: NSSize) -> (NSWindow, NSHostingView<V>) {
        let hosting = NSHostingView(rootView: view)
        let window = UIHarnessWindow(contentRect: NSRect(origin: .zero, size: size),
                                     styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.contentView = hosting
        window.setContentSize(size)
        window.layoutIfNeeded()
        hosting.layoutSubtreeIfNeeded()
        // Let SwiftUI settle its first pass.
        RunLoop.current.run(until: Date().addingTimeInterval(0.4))
        hosting.layoutSubtreeIfNeeded()
        return (window, hosting)
    }

    private func snapshot(_ view: NSView, name: String) {
        guard let dir = ProcessInfo.processInfo.environment["S71_SHOT_DIR"],
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return }
        view.cacheDisplay(in: view.bounds, to: rep)
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        if let png = rep.representation(using: .png, properties: [:]) {
            try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        }
    }

    func testAddSheetShrinksToSmallDisplayAndKeepsFooter() {
        let model = makeModel()
        defer { try? model.accountStore.deleteAll() }
        let (window, hosting) = host(AccountSetupSheet(model: model), size: NSSize(width: 420, height: 300))
        snapshot(hosting, name: "add-account-sheet-420x300")
        // The sheet's minimum (fitting) height must be small: the form scrolls.
        XCTAssertLessThanOrEqual(window.contentMinSize.height, 320,
                                 "form must scroll so the sheet fits a short display")
        withExtendedLifetime(window) {}
    }

    private static func findDragHandle(in view: NSView?) -> SheetDragBackgroundView? {
        guard let view else { return nil }
        if let handle = view as? SheetDragBackgroundView, handle.mouseDownCanMoveWindow { return handle }
        for sub in view.subviews { if let found = findDragHandle(in: sub) { return found } }
        return nil
    }

    /// The policy must stick on a window attached as a real sheet, not just a panel.
    func testAddSheetAttachedAsRealSheetIsMovableAndResizable() {
        let model = makeModel()
        defer { try? model.accountStore.deleteAll() }
        let parent = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 700, height: 500),
                              styleMask: [.titled], backing: .buffered, defer: false)
        let controller = NSHostingController(rootView: AccountSetupSheet(model: model))
        let sheet = NSWindow(contentViewController: controller)
        parent.beginSheet(sheet)
        RunLoop.current.run(until: Date().addingTimeInterval(0.5))
        XCTAssertTrue(parent.attachedSheet === sheet, "sheet must be attached")
        XCTAssertTrue(sheet.isMovable)
        // AppKit ignores isMovableByWindowBackground on attached sheets, so the
        // view installs an explicit drag handle (performDrag) behind the form.
        XCTAssertNotNil(Self.findDragHandle(in: sheet.contentView),
                        "sheet must carry the drag-handle background")
        XCTAssertTrue(sheet.styleMask.contains(.resizable))
        XCTAssertLessThanOrEqual(sheet.contentMinSize.height, SheetLayout.minHeight)
        parent.endSheet(sheet)
    }

    func testSettingsGeneralFitsShortWindow() {
        let model = makeModel()
        defer { try? model.accountStore.deleteAll() }
        let prefs = Preferences(defaults: UserDefaults(suiteName: "castellum-s71-\(UUID().uuidString)")!)
        let (window, hosting) = host(SettingsView(prefs: prefs, model: model), size: NSSize(width: 560, height: 340))
        snapshot(hosting, name: "settings-general-560x340")
        XCTAssertLessThanOrEqual(window.contentMinSize.height, 340,
                                 "Settings minimum height must leave room for the scrolling General form")
        withExtendedLifetime(window) {}
    }
}
