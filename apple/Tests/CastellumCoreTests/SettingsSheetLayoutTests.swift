// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import AppKit
import XCTest
@testable import Castellum

/// S71: layout contracts for Settings and the
/// Add/Re-authenticate sheet. SwiftUI view trees are not introspectable
/// without a host, so the assertable parts are the pure layout constants and
/// the AppKit window policy the sheets apply; the rest is screenshot-verified.
final class SettingsSheetLayoutTests: XCTestCase {
    func testSettingsWindowIsResizableWithoutFixedCeiling() {
        XCTAssertLessThanOrEqual(SettingsLayout.minHeight, 380)
        XCTAssertGreaterThan(SettingsLayout.idealHeight, SettingsLayout.minHeight)
        XCTAssertEqual(SettingsLayout.maxHeight, .infinity, "Settings must not cap the height")
        XCTAssertGreaterThan(SettingsLayout.maxWidth, SettingsLayout.idealWidth)
    }

    func testSignatureEditorIsBoundedSoItCannotCoverTheList() {
        XCTAssertLessThanOrEqual(SettingsLayout.signatureMaxHeight, 120)
        XCTAssertGreaterThanOrEqual(SettingsLayout.accountListMinHeight, 100)
        XCTAssertLessThanOrEqual(SettingsLayout.accountDetailMaxHeight, 220)
    }

    func testSheetFitsSmallDisplaysAndScrollsBody() {
        XCTAssertLessThanOrEqual(SheetLayout.minHeight, 320, "must fit small displays")
        XCTAssertGreaterThan(SheetLayout.idealHeight, SheetLayout.minHeight)
        XCTAssertEqual(SheetLayout.width, 420)
    }

    @MainActor
    func testSheetWindowPolicyMakesWindowMovableAndResizable() {
        let window = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 420, height: 400),
            styleMask: [.titled], backing: .buffered, defer: true)
        window.isMovable = false
        window.isMovableByWindowBackground = false
        SheetWindowPolicy.apply(to: window)
        XCTAssertTrue(window.isMovable)
        XCTAssertTrue(window.isMovableByWindowBackground)
        XCTAssertTrue(window.styleMask.contains(.resizable))
        XCTAssertEqual(window.contentMinSize.height, SheetLayout.minHeight)
    }

    /// GUI-1: SwiftUI configures the window of a `.sheet` AFTER it installs
    /// the content view, wiping the style mask and the minimum size the drag handle set when
    /// it was attached (seen live: `scripts/ui-check.sh gui1-add-account-sheet` reported
    /// "styleMask lacks .resizable" and "no contentMinSize"). The policy must come back.
    @MainActor
    func testSheetPolicyIsRestoredAfterTheWindowIsReconfigured() {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 420, height: 400),
            styleMask: [.titled], backing: .buffered, defer: true)
        let handle = SheetDragBackgroundView()
        window.contentView?.addSubview(handle)
        XCTAssertTrue(window.styleMask.contains(.resizable), "applied when the handle is attached")

        // What SwiftUI does to the sheet's window afterwards.
        window.styleMask.remove(.resizable)
        window.contentMinSize = .zero
        NotificationCenter.default.post(name: NSWindow.didUpdateNotification, object: window)

        XCTAssertTrue(window.styleMask.contains(.resizable))
        XCTAssertEqual(window.contentMinSize, NSSize(width: SheetLayout.width, height: SheetLayout.minHeight))
        withExtendedLifetime(handle) {}
    }

    // MARK: GUI-1 / GUI-3

    func testSheetOpensAtFullContentHeightOnALargeDisplay() {
        XCTAssertEqual(SheetLayout.idealHeight(visibleScreenHeight: 1400), SheetLayout.fullContentHeight)
        XCTAssertEqual(SheetLayout.idealHeight(visibleScreenHeight: nil), SheetLayout.fullContentHeight)
    }

    func testSheetShrinksToASmallDisplayButNeverBelowItsMinimum() {
        XCTAssertEqual(SheetLayout.idealHeight(visibleScreenHeight: 700), 700 - SheetLayout.screenMargin)
        XCTAssertEqual(SheetLayout.idealHeight(visibleScreenHeight: 200), SheetLayout.minHeight)
    }

    @MainActor
    func testSettingsWindowPolicyMakesWindowResizableAndRemembered() {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 480),
            styleMask: [.titled, .closable], backing: .buffered, defer: true)
        XCTAssertFalse(window.styleMask.contains(.resizable))
        SettingsWindowPolicy.apply(to: window)
        XCTAssertTrue(window.styleMask.contains(.resizable))
        XCTAssertTrue(window.isMovable)
        XCTAssertEqual(window.contentMinSize.width, SettingsLayout.minWidth)
        XCTAssertEqual(window.frameAutosaveName, SettingsWindowPolicy.autosaveName)
        // Re-applying is idempotent and leaves the name alone.
        SettingsWindowPolicy.apply(to: window)
        XCTAssertEqual(window.frameAutosaveName, SettingsWindowPolicy.autosaveName)
    }
}
