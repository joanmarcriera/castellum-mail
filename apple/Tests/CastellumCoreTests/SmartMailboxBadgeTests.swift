// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S41 smart mailbox sidebar badge formatting (pure helper).
import XCTest
@testable import Castellum

final class SmartMailboxBadgeTests: XCTestCase {
    func testZeroUnreadHidesTheBadge() {
        XCTAssertNil(SmartMailboxBadge.label(unread: 0, capped: false))
    }

    func testUncappedShowsTheExactCount() {
        XCTAssertEqual(SmartMailboxBadge.label(unread: 7, capped: false), "7")
    }

    func testCappedShowsAPlusSuffix() {
        XCTAssertEqual(SmartMailboxBadge.label(unread: 2000, capped: true), "2000+")
    }
}
