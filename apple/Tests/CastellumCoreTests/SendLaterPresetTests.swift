// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - Send Later preset arithmetic (HP1). Pure date math: the calendar
// (and so the time zone and its daylight-saving rules) and the clock are
// injected, so nothing here depends on the machine's zone or on today's date.
import XCTest
@testable import Castellum

final class SendLaterPresetTests: XCTestCase {
    private var london: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "Europe/London")!
        c.firstWeekday = 2
        return c
    }()

    /// A local (London) wall-clock instant.
    private func local(_ y: Int, _ m: Int, _ d: Int, _ h: Int, _ min: Int = 0) -> Date {
        london.date(from: DateComponents(year: y, month: m, day: d, hour: h, minute: min))!
    }

    private func utc(_ y: Int, _ m: Int, _ d: Int, _ h: Int, _ min: Int = 0) -> Date {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "UTC")!
        return c.date(from: DateComponents(year: y, month: m, day: d, hour: h, minute: min))!
    }

    private func parts(_ date: Date) -> DateComponents {
        london.dateComponents([.year, .month, .day, .hour, .minute, .weekday], from: date)
    }

    // MARK: Ordinary days

    func testInOneHourIsAnExactHour() {
        let now = local(2026, 10, 1, 9, 15)
        XCTAssertEqual(SendLaterPreset.inOneHour.sendDate(calendar: london, now: now),
                       now.addingTimeInterval(3600))
    }

    func testThisEveningIsSixPmToday() {
        let now = local(2026, 10, 1, 9, 15) // a Thursday
        let d = SendLaterPreset.thisEvening.sendDate(calendar: london, now: now)
        XCTAssertEqual(parts(d).day, 1)
        XCTAssertEqual(parts(d).hour, 18)
        XCTAssertEqual(parts(d).minute, 0)
    }

    func testThisEveningAfterSixPmRollsToTomorrowEvening() {
        let now = local(2026, 10, 1, 19, 0)
        let d = SendLaterPreset.thisEvening.sendDate(calendar: london, now: now)
        XCTAssertEqual(parts(d).day, 2)
        XCTAssertEqual(parts(d).hour, 18)
    }

    func testTomorrowMorningIsEightAmTomorrow() {
        let now = local(2026, 10, 1, 23, 50)
        let d = SendLaterPreset.tomorrowMorning.sendDate(calendar: london, now: now)
        XCTAssertEqual(parts(d).day, 2)
        XCTAssertEqual(parts(d).hour, 8)
        XCTAssertEqual(parts(d).minute, 0)
    }

    func testMondayMorningIsTheNextMondayAtEight() {
        let thursday = local(2026, 10, 1, 9, 0)
        let d = SendLaterPreset.mondayMorning.sendDate(calendar: london, now: thursday)
        XCTAssertEqual(parts(d).weekday, 2)
        XCTAssertEqual(parts(d).day, 5)
        XCTAssertEqual(parts(d).hour, 8)
    }

    func testMondayMorningOnAMondayMeansNextWeek() {
        let monday = local(2026, 10, 5, 7, 0) // before 08:00, still next Monday
        let d = SendLaterPreset.mondayMorning.sendDate(calendar: london, now: monday)
        XCTAssertEqual(parts(d).day, 12)
        XCTAssertEqual(parts(d).hour, 8)
    }

    func testMondayMorningOnASundayIsTomorrow() {
        let sunday = local(2026, 10, 4, 22, 0)
        let d = SendLaterPreset.mondayMorning.sendDate(calendar: london, now: sunday)
        XCTAssertEqual(parts(d).day, 5)
        XCTAssertEqual(parts(d).hour, 8)
    }

    func testCustomIsReturnedUnchanged() {
        let when = local(2026, 12, 25, 6, 30)
        XCTAssertEqual(SendLaterPreset.custom(when).sendDate(calendar: london, now: Date()), when)
    }

    // MARK: Daylight saving (Europe/London: forward 2026-03-29, back 2026-10-25)

    func testTomorrowMorningAcrossSpringForwardKeepsEightLocal() {
        // Sat 28 Mar 20:00 GMT -> Sun 29 Mar 08:00 BST: only 11 real hours.
        let now = local(2026, 3, 28, 20, 0)
        let d = SendLaterPreset.tomorrowMorning.sendDate(calendar: london, now: now)
        XCTAssertEqual(parts(d).day, 29)
        XCTAssertEqual(parts(d).hour, 8, "wall-clock 08:00 survives the change")
        XCTAssertEqual(d, utc(2026, 3, 29, 7, 0), "08:00 BST is 07:00 UTC")
        XCTAssertEqual(d.timeIntervalSince(now), 11 * 3600)
    }

    func testInOneHourAcrossSpringForwardIsStillExactlyOneHour() {
        // 00:30 GMT + 1 h = 02:30 BST (01:30 local does not exist).
        let now = local(2026, 3, 29, 0, 30)
        let d = SendLaterPreset.inOneHour.sendDate(calendar: london, now: now)
        XCTAssertEqual(d, utc(2026, 3, 29, 1, 30))
        XCTAssertEqual(parts(d).hour, 2)
        XCTAssertEqual(parts(d).minute, 30)
    }

    func testThisEveningOnSpringForwardDayIsSixPmBst() {
        let now = local(2026, 3, 29, 10, 0)
        let d = SendLaterPreset.thisEvening.sendDate(calendar: london, now: now)
        XCTAssertEqual(parts(d).hour, 18)
        XCTAssertEqual(d, utc(2026, 3, 29, 17, 0), "18:00 BST is 17:00 UTC")
    }

    func testMondayMorningAcrossFallBackKeepsEightLocal() {
        // Sat 24 Oct 12:00 BST -> Mon 26 Oct 08:00 GMT: 45 real hours, not 44.
        let now = local(2026, 10, 24, 12, 0)
        let d = SendLaterPreset.mondayMorning.sendDate(calendar: london, now: now)
        XCTAssertEqual(parts(d).day, 26)
        XCTAssertEqual(parts(d).weekday, 2)
        XCTAssertEqual(parts(d).hour, 8)
        XCTAssertEqual(d, utc(2026, 10, 26, 8, 0), "08:00 GMT is 08:00 UTC")
        XCTAssertEqual(d.timeIntervalSince(now), 45 * 3600)
    }

    func testTomorrowMorningAcrossFallBackKeepsEightLocal() {
        // Sat 24 Oct 22:00 BST -> Sun 25 Oct 08:00 GMT: 11 real hours.
        let now = local(2026, 10, 24, 22, 0)
        let d = SendLaterPreset.tomorrowMorning.sendDate(calendar: london, now: now)
        XCTAssertEqual(parts(d).day, 25)
        XCTAssertEqual(parts(d).hour, 8)
        XCTAssertEqual(d, utc(2026, 10, 25, 8, 0))
        XCTAssertEqual(d.timeIntervalSince(now), 11 * 3600)
    }

    func testPresetsAreAlwaysInTheFuture() {
        for offsetHours in stride(from: 0, to: 24 * 14, by: 7) {
            let now = utc(2026, 3, 25, 0, 0).addingTimeInterval(Double(offsetHours) * 3600)
            for preset in SendLaterPreset.standardCases {
                XCTAssertGreaterThan(preset.sendDate(calendar: london, now: now), now,
                                     "\(preset) from \(now)")
            }
        }
    }

    // MARK: Display and catalogue

    func testDisplayUsesTheGivenTimeZone() {
        let epoch = Int64(utc(2026, 3, 29, 7, 0).timeIntervalSince1970)
        let london = sendLaterDisplay(epoch, locale: Locale(identifier: "en_GB"),
                                      timeZone: TimeZone(identifier: "Europe/London")!)
        XCTAssertTrue(london.contains("08:00"), london)
    }

    func testSendLaterStringsExistInEnglishSpanishAndCatalan() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/Castellum/Resources/Localizable.xcstrings")
        let root = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        let strings = root["strings"] as! [String: [String: Any]]
        let keys = [
            "In one hour", "This evening, 18:00", "Tomorrow morning, 08:00",
            "Monday morning, 08:00", "Custom…", "Send Later…", "Send Later", "Schedule",
            "Reschedule", "Send Again", "Back to Drafts", "Scheduled message",
            "Scheduled for %@",
            "Needs confirmation — it was due %@ while Castellum was closed.",
            "May have been sent — it was being delivered when Castellum quit. Check Sent before sending again.",
        ]
        for key in keys {
            let locs = strings[key]?["localizations"] as? [String: Any]
            XCTAssertEqual(Set(locs?.keys.map { $0 } ?? []), ["en", "es", "ca"], key)
        }
        // The catalogue as a whole stays complete.
        for (key, entry) in strings {
            guard let locs = entry["localizations"] as? [String: Any] else { continue }
            XCTAssertTrue(Set(locs.keys).isSuperset(of: ["en", "es", "ca"]), "incomplete: \(key)")
        }
    }
}
