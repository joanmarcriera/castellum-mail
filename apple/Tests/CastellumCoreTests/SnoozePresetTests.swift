// Castellum - S43 SnoozePreset date math. Pins a fixed Gregorian calendar +
// UTC time zone so the tests are deterministic regardless of the machine's
// locale/timezone/DST.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class SnoozePresetTests: XCTestCase {
    private var calendar: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "UTC")!
        return c
    }()

    private func date(_ y: Int, _ m: Int, _ d: Int, _ h: Int = 0, _ min: Int = 0) -> Date {
        calendar.date(from: DateComponents(year: y, month: m, day: d, hour: h, minute: min))!
    }

    func testLaterTodayBeforeSixPmReturnsSixPmToday() {
        let now = date(2026, 9, 18, 10, 0) // Friday 10:00
        let due = SnoozePreset.laterToday.dueDate(calendar: calendar, now: now)
        XCTAssertEqual(due, date(2026, 9, 18, 18, 0))
    }

    func testLaterTodayAfterSixPmReturnsThreeHoursFromNow() {
        let now = date(2026, 9, 18, 20, 0) // Friday 20:00
        let due = SnoozePreset.laterToday.dueDate(calendar: calendar, now: now)
        XCTAssertEqual(due, now.addingTimeInterval(3 * 3600))
    }

    func testTomorrowReturnsEightAmTheNextDay() {
        let now = date(2026, 9, 18, 22, 0) // Friday 22:00
        let due = SnoozePreset.tomorrow.dueDate(calendar: calendar, now: now)
        XCTAssertEqual(due, date(2026, 9, 19, 8, 0))
    }

    func testThisWeekendReturnsTheComingSaturdayAtNineAm() {
        let now = date(2026, 9, 18, 10, 0) // Friday
        let due = SnoozePreset.thisWeekend.dueDate(calendar: calendar, now: now)
        XCTAssertEqual(due, date(2026, 9, 19, 9, 0)) // Saturday
    }

    func testNextWeekReturnsTheComingMondayAtEightAm() {
        let now = date(2026, 9, 18, 10, 0) // Friday
        let due = SnoozePreset.nextWeek.dueDate(calendar: calendar, now: now)
        XCTAssertEqual(due, date(2026, 9, 21, 8, 0)) // Monday
    }

    func testLaterTodayPastNinePmDoesNotWakeTheMessageAfterMidnight() {
        let now = date(2026, 9, 18, 22, 30)
        XCTAssertEqual(SnoozePreset.laterToday.dueDate(calendar: calendar, now: now), date(2026, 9, 19, 8, 0))
        // boundary: 20:59 + 3h = 23:59 still today; 21:00 + 3h = midnight is not
        let before = date(2026, 9, 18, 20, 59)
        XCTAssertEqual(SnoozePreset.laterToday.dueDate(calendar: calendar, now: before), before.addingTimeInterval(3 * 3600))
        XCTAssertEqual(SnoozePreset.laterToday.dueDate(calendar: calendar, now: date(2026, 9, 18, 21, 0)), date(2026, 9, 19, 8, 0))
    }

    func testNextWeekOnAMondayMeansTheFollowingMonday() {
        let want = date(2026, 9, 28, 8, 0)
        XCTAssertEqual(SnoozePreset.nextWeek.dueDate(calendar: calendar, now: date(2026, 9, 21, 7, 0)), want)
        XCTAssertEqual(SnoozePreset.nextWeek.dueDate(calendar: calendar, now: date(2026, 9, 21, 9, 0)), want)
        XCTAssertEqual(SnoozePreset.nextWeek.dueDate(calendar: calendar, now: date(2026, 9, 20, 23, 0)), date(2026, 9, 21, 8, 0)) // Sunday 23:00
    }

    func testThisWeekendOnTheWeekend() {
        let p = SnoozePreset.thisWeekend
        XCTAssertEqual(p.dueDate(calendar: calendar, now: date(2026, 9, 19, 7, 0)), date(2026, 9, 19, 9, 0))   // Sat early
        XCTAssertEqual(p.dueDate(calendar: calendar, now: date(2026, 9, 19, 11, 0)), date(2026, 9, 20, 9, 0))  // Sat later -> Sunday
        XCTAssertEqual(p.dueDate(calendar: calendar, now: date(2026, 9, 20, 7, 0)), date(2026, 9, 20, 9, 0))   // Sun early
        XCTAssertEqual(p.dueDate(calendar: calendar, now: date(2026, 9, 20, 10, 0)), date(2026, 9, 26, 9, 0))  // Sun late -> next Sat
        XCTAssertEqual(p.dueDate(calendar: calendar, now: date(2026, 9, 21, 10, 0)), date(2026, 9, 26, 9, 0))  // Mon
    }

    func testCustomReturnsExactlyTheGivenDate() {
        let picked = date(2027, 1, 1, 9, 30)
        let due = SnoozePreset.custom(picked).dueDate(calendar: calendar, now: date(2026, 9, 18))
        XCTAssertEqual(due, picked)
    }

    func testTitlesAreBritishEnglish() {
        XCTAssertEqual(SnoozePreset.laterToday.title, "Later today")
        XCTAssertEqual(SnoozePreset.tomorrow.title, "Tomorrow")
        XCTAssertEqual(SnoozePreset.thisWeekend.title, "This weekend")
        XCTAssertEqual(SnoozePreset.nextWeek.title, "Next week")
        XCTAssertEqual(SnoozePreset.custom(Date()).title, "Custom…")
    }
}
