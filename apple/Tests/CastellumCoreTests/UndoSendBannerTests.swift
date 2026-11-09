// Castellum - Tests for the undo-send preference default and the after-Send
// banner's pure state reducer (S25).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class UndoSendPreferenceTests: XCTestCase {
    private func freshDefaults() -> UserDefaults {
        let suite = "undo-send-prefs-\(UUID().uuidString)"
        return UserDefaults(suiteName: suite)!
    }

    func testDefaultUndoSendDelayIsTenSeconds() {
        let prefs = Preferences(defaults: freshDefaults())
        XCTAssertEqual(prefs.undoSendDelay, .ten)
        XCTAssertEqual(prefs.undoSendDelay.seconds, 10)
    }

    func testUndoSendDelayPersistsAcrossInstances() {
        let defaults = freshDefaults()
        let prefs = Preferences(defaults: defaults)
        prefs.undoSendDelay = .thirty

        let reloaded = Preferences(defaults: defaults)
        XCTAssertEqual(reloaded.undoSendDelay, .thirty)
    }

    func testOffDisablesTheDelayEntirely() {
        let prefs = Preferences(defaults: freshDefaults())
        prefs.undoSendDelay = .off
        XCTAssertEqual(prefs.undoSendDelay.seconds, 0)
        XCTAssertEqual(prefs.undoSendDelay.label, "Off")
    }

    func testAllCasesMatchTheBriefOffFiveTenTwentyThirtyDefaultTen() {
        XCTAssertEqual(
            UndoSendDelay.allCases.map(\.seconds),
            [0, 5, 10, 20, 30]
        )
    }
}

final class UndoSendBannerStateTests: XCTestCase {
    func testSendShowsTheBannerWithTheFullCountdown() {
        var state = UndoSendBannerState()
        state.send(draftId: 1, accountId: "acc", delaySeconds: 10)
        XCTAssertTrue(state.isVisible)
        XCTAssertEqual(state.pending?.draftId, 1)
        XCTAssertEqual(state.pending?.accountId, "acc")
        XCTAssertEqual(state.pending?.remainingSeconds, 10)
        XCTAssertEqual(state.pending?.totalSeconds, 10)
    }

    func testSendWithZeroOrNegativeDelayNeverShowsABanner() {
        // Undo send "Off" (0) must never surface a banner — this is the
        // no-behaviour-change path, so a stray call with a non-positive delay
        // is a no-op rather than a banner stuck at "0s".
        var state = UndoSendBannerState()
        state.send(draftId: 1, accountId: "acc", delaySeconds: 0)
        XCTAssertFalse(state.isVisible)
        state.send(draftId: 1, accountId: "acc", delaySeconds: -5)
        XCTAssertFalse(state.isVisible)
    }

    func testTickCountsDownAndDismissesAtZero() {
        var state = UndoSendBannerState()
        state.send(draftId: 1, accountId: "acc", delaySeconds: 2)
        state.tick()
        XCTAssertEqual(state.pending?.remainingSeconds, 1)
        XCTAssertTrue(state.isVisible)
        state.tick()
        XCTAssertNil(state.pending, "dismissed once the countdown reaches zero")
        XCTAssertFalse(state.isVisible)
    }

    func testTickOnAnEmptyStateIsANoOp() {
        var state = UndoSendBannerState()
        state.tick()
        XCTAssertFalse(state.isVisible)
    }

    func testDismissClearsAnyPendingBanner() {
        var state = UndoSendBannerState()
        state.send(draftId: 1, accountId: "acc", delaySeconds: 10)
        state.dismiss()
        XCTAssertFalse(state.isVisible)
    }

    func testSendReplacesAnEarlierStillCountingDownBanner() {
        // A second Send while one banner is up REPLACES it — see the type's
        // doc comment (the earlier delayed send's own drain keeps running,
        // only its banner is no longer shown).
        var state = UndoSendBannerState()
        state.send(draftId: 1, accountId: "acc", delaySeconds: 10)
        state.tick()
        XCTAssertEqual(state.pending?.remainingSeconds, 9)

        state.send(draftId: 2, accountId: "acc", delaySeconds: 5)
        XCTAssertEqual(state.pending?.draftId, 2)
        XCTAssertEqual(state.pending?.remainingSeconds, 5)
    }
}
