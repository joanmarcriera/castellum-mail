// Castellum - tests for the main-window screen clamp.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `clampedWindowFrame(current:visible:)` is the pure math behind
// WindowScreenClamper: after zoom or a layout-driven resize (sidebar /
// reading-pane toggles), the main window frame must fit entirely inside the
// current screen's `visibleFrame` — width, height, AND origin (title bar
// stays on-screen).

import XCTest
@testable import Castellum

final class WindowClampTests: XCTestCase {
    // MARK: Layout fight guard (macOS 26 crash feedback)
    //
    // During a sidebar drag SwiftUI grows the window from inside its layout
    // pass; if the clamp shrinks it straight back, the two re-trigger each
    // other until AppKit throws "more Update Constraints in Window passes
    // than there are views". The guard yields on the immediate repeat.

    func testGuardClampsFirstOversizeFrame() {
        var guardState = ClampFightGuard()
        XCTAssertFalse(guardState.shouldYield(to: NSSize(width: 1527, height: 800)))
    }

    func testGuardYieldsWhenTheSameOversizeFrameComesStraightBack() {
        var guardState = ClampFightGuard()
        let size = NSSize(width: 1527, height: 800)
        XCTAssertFalse(guardState.shouldYield(to: size))
        XCTAssertTrue(guardState.shouldYield(to: size), "second identical request in one turn is the fight")
        XCTAssertTrue(guardState.shouldYield(to: size), "keeps yielding until the turn ends")
    }

    func testGuardClampsAgainAfterTheTurnEnds() {
        var guardState = ClampFightGuard()
        let size = NSSize(width: 1527, height: 800)
        XCTAssertFalse(guardState.shouldYield(to: size))
        guardState.endTurn()
        XCTAssertFalse(guardState.shouldYield(to: size), "a later zoom to the same size is a fresh request")
    }

    func testGuardClampsADifferentOversizeFrame() {
        var guardState = ClampFightGuard()
        XCTAssertFalse(guardState.shouldYield(to: NSSize(width: 1527, height: 800)))
        XCTAssertFalse(guardState.shouldYield(to: NSSize(width: 1600, height: 800)))
    }

    /// A MacBook-Pro-ish visible frame (menu bar already subtracted).
    private let visible = NSRect(x: 0, y: 0, width: 1512, height: 944)

    // MARK: Oversized frames are shrunk and repositioned inside

    func testWiderThanScreenIsShrunkToVisibleWidth() {
        // The reproduced bug: zoomed to full width, then the sidebar
        // re-expands and adds its width on top (1512 + 188).
        let current = NSRect(x: 0, y: 0, width: 1700, height: 900)
        let clamped = clampedWindowFrame(current: current, visible: visible)
        XCTAssertEqual(clamped, NSRect(x: 0, y: 0, width: 1512, height: 900))
    }

    func testTallerThanScreenIsShrunkToVisibleHeight() {
        let current = NSRect(x: 100, y: -50, width: 1000, height: 1200)
        let clamped = clampedWindowFrame(current: current, visible: visible)
        XCTAssertEqual(clamped, NSRect(x: 100, y: 0, width: 1000, height: 944))
    }

    func testLargerThanScreenInBothDimensionsFillsVisibleFrame() {
        let current = NSRect(x: -20, y: -20, width: 2000, height: 1400)
        let clamped = clampedWindowFrame(current: current, visible: visible)
        XCTAssertEqual(clamped, visible)
    }

    // MARK: Frames already inside are untouched

    func testFrameInsideVisibleIsUnchanged() {
        let current = NSRect(x: 200, y: 100, width: 1100, height: 700)
        let clamped = clampedWindowFrame(current: current, visible: visible)
        XCTAssertEqual(clamped, current)
    }

    func testFrameExactlyFillingVisibleIsUnchanged() {
        let clamped = clampedWindowFrame(current: visible, visible: visible)
        XCTAssertEqual(clamped, visible)
    }

    // MARK: Off-screen origins are moved back on-screen

    func testFrameHangingOffRightEdgeIsMovedLeft() {
        let current = NSRect(x: 1400, y: 100, width: 600, height: 500)
        let clamped = clampedWindowFrame(current: current, visible: visible)
        XCTAssertEqual(clamped, NSRect(x: 912, y: 100, width: 600, height: 500))
    }

    func testFrameHangingOffLeftEdgeIsMovedRight() {
        let current = NSRect(x: -300, y: 100, width: 600, height: 500)
        let clamped = clampedWindowFrame(current: current, visible: visible)
        XCTAssertEqual(clamped, NSRect(x: 0, y: 100, width: 600, height: 500))
    }

    func testFrameAboveVisibleTopIsPulledDownSoTitleBarStaysOnScreen() {
        // maxY beyond the visible top would push the title bar under the
        // menu bar; the frame must be pulled down so maxY == visible.maxY.
        let current = NSRect(x: 100, y: 600, width: 800, height: 500)
        let clamped = clampedWindowFrame(current: current, visible: visible)
        XCTAssertEqual(clamped, NSRect(x: 100, y: 444, width: 800, height: 500))
    }

    // MARK: Degenerate visible frames never produce a zero/shrunken window

    func testZeroSizeVisibleFrameReturnsCurrentUnchanged() {
        // A screen mid-reconfiguration can report a zero visibleFrame;
        // clamping into it would setFrame(.zero) → invisible window
        // (contentMinSize does not constrain programmatic setFrame).
        let current = NSRect(x: 100, y: 100, width: 1200, height: 800)
        let clamped = clampedWindowFrame(current: current, visible: .zero)
        XCTAssertEqual(clamped, current)
    }

    func testZeroWidthVisibleFrameReturnsCurrentUnchanged() {
        let current = NSRect(x: 100, y: 100, width: 1200, height: 800)
        let degenerate = NSRect(x: 0, y: 0, width: 0, height: 944)
        XCTAssertEqual(clampedWindowFrame(current: current, visible: degenerate),
                       current)
    }

    func testZeroHeightVisibleFrameReturnsCurrentUnchanged() {
        let current = NSRect(x: 100, y: 100, width: 1200, height: 800)
        let degenerate = NSRect(x: 0, y: 0, width: 1512, height: 0)
        XCTAssertEqual(clampedWindowFrame(current: current, visible: degenerate),
                       current)
    }

    // MARK: Non-zero-origin screens (secondary display) still clamp correctly

    func testClampRespectsVisibleFrameOrigin() {
        // A display arranged to the left of the main one: negative X origin.
        let secondary = NSRect(x: -1920, y: 200, width: 1920, height: 1055)
        let current = NSRect(x: -2000, y: 100, width: 2100, height: 1200)
        let clamped = clampedWindowFrame(current: current, visible: secondary)
        XCTAssertEqual(clamped, secondary)
    }
}
