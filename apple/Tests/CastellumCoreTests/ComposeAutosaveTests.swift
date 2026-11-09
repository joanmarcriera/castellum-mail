// Castellum - unit tests for the pure compose-autosave debounce policy.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class ComposeAutosaveTests: XCTestCase {
    func testNotDueBeforeTheInterval() {
        let scheduler = ComposeAutosaveScheduler(debounceInterval: 2.0)
        let editedAt = Date()
        XCTAssertFalse(scheduler.isDue(lastEditAt: editedAt, now: editedAt.addingTimeInterval(1.5)))
    }

    func testDueExactlyAtTheInterval() {
        let scheduler = ComposeAutosaveScheduler(debounceInterval: 2.0)
        let editedAt = Date()
        XCTAssertTrue(scheduler.isDue(lastEditAt: editedAt, now: editedAt.addingTimeInterval(2.0)))
    }

    func testDueWellAfterTheInterval() {
        let scheduler = ComposeAutosaveScheduler(debounceInterval: 2.0)
        let editedAt = Date()
        XCTAssertTrue(scheduler.isDue(lastEditAt: editedAt, now: editedAt.addingTimeInterval(10)))
    }
}
