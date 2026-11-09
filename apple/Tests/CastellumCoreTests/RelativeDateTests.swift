// Castellum - relativeDate calendar-day boundaries (audit #3454).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class RelativeDateTests: XCTestCase {
    private func epoch(_ y: Int, _ m: Int, _ d: Int, _ h: Int, _ min: Int = 0) -> Int64 {
        let c = Calendar.current
        return Int64(c.date(from: DateComponents(year: y, month: m, day: d, hour: h, minute: min))!
            .timeIntervalSince1970)
    }

    private func now(_ y: Int, _ m: Int, _ d: Int, _ h: Int) -> Date {
        Date(timeIntervalSince1970: TimeInterval(epoch(y, m, d, h)))
    }

    private func format(_ pattern: String, _ epoch: Int64) -> String {
        let f = DateFormatter()
        f.dateFormat = pattern
        return f.string(from: Date(timeIntervalSince1970: TimeInterval(epoch)))
    }

    func testAMessageFromLastMondayEveningIsNotLabelledMon() {
        // Monday 19 Oct 2026 10:00; message last Monday 12 Oct 18:00 (6 full 24 h periods ago).
        let last = epoch(2026, 10, 12, 18)
        XCTAssertEqual(relativeDate(last, now: now(2026, 10, 19, 10)), format("d MMM", last))
    }

    func testAMessageFromLastTuesdayEveningIsLabelledTue() {
        let tue = epoch(2026, 10, 13, 18)
        XCTAssertEqual(relativeDate(tue, now: now(2026, 10, 19, 10)), format("EEE", tue))
    }

    func testTodayAndYesterdayAreUnchanged() {
        let today = epoch(2026, 10, 19, 8, 5)
        XCTAssertEqual(relativeDate(today, now: now(2026, 10, 19, 10)), format("HH:mm", today))
        XCTAssertEqual(relativeDate(epoch(2026, 10, 18, 23), now: now(2026, 10, 19, 10)), "Yesterday")
    }
}
