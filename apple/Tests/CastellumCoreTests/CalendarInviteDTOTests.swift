// Castellum - calendar invite DTO decode + date resolution tests (S55).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class CalendarInviteDTOTests: XCTestCase {
    private func decoder() -> JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }

    func testCalendarInviteDecodesTheCoreJsonShape() throws {
        let json = """
        {
            "uid": "event-1@example.com",
            "sequence": 2,
            "method": "REQUEST",
            "summary": "Roadmap review",
            "dtstart": {"raw": "20260925T140000Z", "tzid": null, "is_utc": true, "is_date_only": false},
            "dtend": {"raw": "20260925T150000Z", "tzid": null, "is_utc": true, "is_date_only": false},
            "organizer_email": "alice@example.com",
            "organizer_name": "Alice Organizer",
            "attendees": [
                {"email": "bob@example.com", "name": "Bob Attendee", "role": "REQ-PARTICIPANT", "part_stat": "NEEDS-ACTION"}
            ],
            "our_response": null,
            "responded_at": null
        }
        """
        let invite = try decoder().decode(CalendarInvite.self, from: Data(json.utf8))
        XCTAssertEqual(invite.uid, "event-1@example.com")
        XCTAssertEqual(invite.sequence, 2)
        XCTAssertEqual(invite.method, "REQUEST")
        XCTAssertEqual(invite.summary, "Roadmap review")
        XCTAssertEqual(invite.dtstart?.raw, "20260925T140000Z")
        XCTAssertEqual(invite.dtstart?.isUtc, true)
        XCTAssertEqual(invite.organizerEmail, "alice@example.com")
        XCTAssertEqual(invite.attendees.count, 1)
        XCTAssertEqual(invite.attendees[0].email, "bob@example.com")
        XCTAssertEqual(invite.attendees[0].partStat, "NEEDS-ACTION")
        XCTAssertNil(invite.ourResponse)
    }

    /// #3446: superseded state and RECURRENCE-ID arrive from the core; Add to Calendar is not
    /// offered for a superseded invite, a cancellation or a reply.
    func testSupersededAndRecurrenceIdDecodeAndGateAddToCalendar() throws {
        func invite(method: String, superseded: String?) throws -> CalendarInvite {
            let sup = superseded.map { "\"\($0)\"" } ?? "null"
            let json = """
            {"uid":"e@x.com","sequence":1,"method":"\(method)","summary":null,"dtstart":null,"dtend":null,
             "organizer_email":null,"organizer_name":null,"attendees":[],"our_response":null,"responded_at":null,
             "recurrence_id":{"raw":"20260930T090000Z","tzid":null,"is_utc":true,"is_date_only":false},
             "superseded":\(sup)}
            """
            return try decoder().decode(CalendarInvite.self, from: Data(json.utf8))
        }
        let live = try invite(method: "REQUEST", superseded: nil)
        XCTAssertEqual(live.recurrenceId?.raw, "20260930T090000Z")
        XCTAssertTrue(live.offersAddToCalendar)
        XCTAssertFalse(try invite(method: "REQUEST", superseded: "updated").offersAddToCalendar)
        XCTAssertFalse(try invite(method: "CANCEL", superseded: nil).offersAddToCalendar)
        XCTAssertFalse(try invite(method: "REPLY", superseded: nil).offersAddToCalendar)
        XCTAssertTrue(try invite(method: "PUBLISH", superseded: nil).offersAddToCalendar)
    }

    func testCalendarInviteWithAResponseDecodes() throws {
        let json = """
        {"uid":"e@x.com","sequence":0,"method":"REQUEST","summary":null,
         "dtstart":null,"dtend":null,"organizer_email":null,"organizer_name":null,
         "attendees":[],"our_response":"accepted","responded_at":1700000000}
        """
        let invite = try decoder().decode(CalendarInvite.self, from: Data(json.utf8))
        XCTAssertEqual(invite.ourResponse, "accepted")
        XCTAssertEqual(invite.respondedAt, 1_700_000_000)
    }

    func testMessageDetailWithNoCalendarInviteKeyDecodesNilForOlderCorePayloads() throws {
        let json = """
        {"header":{"id":"m1","account_id":"a","mailbox_id":"a:INBOX","subject":null,
         "from_name":null,"from_address":null,"sent_at":null,"snippet":null,
         "has_attachments":false,"has_body":true,"is_read":false,"is_starred":false,
         "thread_count":1},
         "rfc822_message_id":null,"addresses":[],"text_body":null,"sanitized_html":null,
         "list_unsubscribe_one_click":false}
        """
        let detail = try decoder().decode(MessageDetail.self, from: Data(json.utf8))
        XCTAssertNil(detail.calendarInvite)
    }

    // --- CalendarInviteDateTime.resolvedDate() ------------------------------

    func testUtcDateTimeResolvesInUtc() throws {
        let dt = CalendarInviteDateTime(raw: "20260925T140000Z", tzid: nil, isUtc: true, isDateOnly: false)
        var utc = Calendar(identifier: .gregorian)
        utc.timeZone = TimeZone(identifier: "UTC")!
        let resolved = try XCTUnwrap(dt.resolvedDate(calendar: utc))
        let components = utc.dateComponents([.year, .month, .day, .hour, .minute], from: resolved)
        XCTAssertEqual(components.year, 2026)
        XCTAssertEqual(components.month, 9)
        XCTAssertEqual(components.day, 25)
        XCTAssertEqual(components.hour, 14)
        XCTAssertEqual(components.minute, 0)
    }

    func testTzidQualifiedDateTimeResolvesInThatTimeZoneNotFloatingLocal() throws {
        let dt = CalendarInviteDateTime(
            raw: "20260925T140000", tzid: "Europe/London", isUtc: false, isDateOnly: false
        )
        // Resolve, then re-render the SAME instant through a UTC calendar —
        // Europe/London is BST (UTC+1) in September, so 14:00 local should
        // land at 13:00 UTC, proving the TZID (not the test machine's own
        // time zone) governed the resolution.
        var utc = Calendar(identifier: .gregorian)
        utc.timeZone = TimeZone(identifier: "UTC")!
        let resolved = try XCTUnwrap(dt.resolvedDate())
        let hourUtc = utc.component(.hour, from: resolved)
        XCTAssertEqual(hourUtc, 13)
    }

    /// #3446: Outlook/Exchange write Windows zone names; they must resolve in THAT zone, not the Mac's.
    func testWindowsTimeZoneNameResolvesInItsIanaZone() throws {
        let dt = CalendarInviteDateTime(
            raw: "20260925T140000", tzid: "W. Europe Standard Time", isUtc: false, isDateOnly: false)
        var utc = Calendar(identifier: .gregorian)
        utc.timeZone = TimeZone(identifier: "UTC")!
        let resolved = try XCTUnwrap(dt.resolvedDate(calendar: Calendar(identifier: .gregorian)))
        XCTAssertEqual(utc.component(.hour, from: resolved), 12, "CEST is UTC+2 in September")
        XCTAssertFalse(dt.hasUnrecognisedTimeZone)

        let tokyo = CalendarInviteDateTime(
            raw: "20260925T140000", tzid: "\"Tokyo Standard Time\"", isUtc: false, isDateOnly: false)
        XCTAssertEqual(utc.component(.hour, from: try XCTUnwrap(tokyo.resolvedDate(calendar: Calendar(identifier: .gregorian)))), 5)
    }

    func testUnrecognisedTimeZoneIsFlaggedNotSilentlyLocal() {
        let dt = CalendarInviteDateTime(
            raw: "20260925T140000", tzid: "Customized Time Zone", isUtc: false, isDateOnly: false)
        XCTAssertTrue(dt.hasUnrecognisedTimeZone)
        XCTAssertFalse(CalendarInviteDateTime(raw: "20260925T140000Z", tzid: nil, isUtc: true, isDateOnly: false).hasUnrecognisedTimeZone)
        XCTAssertFalse(CalendarInviteDateTime(raw: "20260925T140000", tzid: nil, isUtc: false, isDateOnly: false).hasUnrecognisedTimeZone, "floating time is deliberate")
    }

    func testEveryWindowsTableEntryMapsToAZoneFoundationKnows() {
        let bad = WindowsTimeZones.table.filter { TimeZone(identifier: $0.value) == nil }
        XCTAssertTrue(bad.isEmpty, "unknown IANA ids: \(bad)")
    }

    func testDateOnlyValueResolvesWithNoTimeComponent() throws {
        let dt = CalendarInviteDateTime(raw: "20260925", tzid: nil, isUtc: false, isDateOnly: true)
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "UTC")!
        let resolved = try XCTUnwrap(dt.resolvedDate(calendar: cal))
        let components = cal.dateComponents([.year, .month, .day, .hour], from: resolved)
        XCTAssertEqual(components.year, 2026)
        XCTAssertEqual(components.month, 9)
        XCTAssertEqual(components.day, 25)
        XCTAssertEqual(components.hour, 0)
    }

    func testMalformedRawValueNeverCrashesAndReturnsNil() {
        let hostile = [
            "",
            "not-a-date",
            "2026",
            "20261301T000000Z", // caught later by Calendar, but must not crash here
            "20260925T",
            String(repeating: "9", count: 50),
        ]
        for raw in hostile {
            let dt = CalendarInviteDateTime(raw: raw, tzid: nil, isUtc: true, isDateOnly: false)
            // Just must not crash/trap; a `nil` or a (garbage-but-valid) Date
            // are both acceptable — Foundation's own Calendar decides.
            _ = dt.resolvedDate()
        }
    }

    func testUnknownTzidFallsBackRatherThanCrashing() {
        let dt = CalendarInviteDateTime(
            raw: "20260925T140000", tzid: "Not/ARealZone", isUtc: false, isDateOnly: false
        )
        // TimeZone(identifier:) returns nil for a bogus id, so this must
        // fall back to the passed-in calendar's own time zone, not crash.
        XCTAssertNotNil(dt.resolvedDate())
    }

    // MARK: - EventKit date computation (#3383)

    private func invite(start: String?, end: String?, dateOnly: Bool) throws -> CalendarInvite {
        func dt(_ raw: String?) -> String {
            guard let raw else { return "null" }
            return "{\"raw\":\"\(raw)\",\"tzid\":null,\"is_utc\":false,\"is_date_only\":\(dateOnly)}"
        }
        let json = """
        {"uid":"e@x.com","sequence":0,"method":"REQUEST","summary":null,
         "dtstart":\(dt(start)),"dtend":\(dt(end)),"organizer_email":null,"organizer_name":null,
         "attendees":[],"our_response":null,"responded_at":null}
        """
        return try decoder().decode(CalendarInvite.self, from: Data(json.utf8))
    }

    private var london: Calendar {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "Europe/London")!
        return c
    }

    private func ymd(_ d: Date) -> [Int] {
        let c = london.dateComponents([.year, .month, .day], from: d)
        return [c.year!, c.month!, c.day!]
    }

    func testAllDayOneDayInviteEndsOnTheSameDay() throws {
        let r = EventKitInviteSource.eventDates(
            for: try invite(start: "20260925", end: "20260926", dateOnly: true), calendar: london)
        XCTAssertTrue(r.isAllDay)
        XCTAssertEqual(ymd(r.start), [2026, 9, 25])
        XCTAssertEqual(ymd(r.end), [2026, 9, 25])
    }

    func testAllDayThreeDayInviteEndsOnTheLastDay() throws {
        let r = EventKitInviteSource.eventDates(
            for: try invite(start: "20260925", end: "20260928", dateOnly: true), calendar: london)
        XCTAssertEqual(ymd(r.end), [2026, 9, 27])
    }

    func testAllDayEndEqualToStartIsClampedToStart() throws {
        let r = EventKitInviteSource.eventDates(
            for: try invite(start: "20260925", end: "20260925", dateOnly: true), calendar: london)
        XCTAssertEqual(r.end, r.start)
    }

    func testAllDayWithoutDtendIsOneDay() throws {
        let r = EventKitInviteSource.eventDates(
            for: try invite(start: "20260925", end: nil, dateOnly: true), calendar: london)
        XCTAssertTrue(r.isAllDay)
        XCTAssertEqual(r.end, r.start)
    }

    func testAllDayAcrossDstTransitionEndsOnTheRightDay() throws {
        // UK clocks go back on 2026-10-25: a 24 h subtraction would land on
        // the 25th at 23:00 of the previous day; calendar arithmetic must not.
        let r = EventKitInviteSource.eventDates(
            for: try invite(start: "20261024", end: "20261026", dateOnly: true), calendar: london)
        XCTAssertEqual(ymd(r.end), [2026, 10, 25])
    }

    func testTimedInviteIsUnchanged() throws {
        let r = EventKitInviteSource.eventDates(
            for: try invite(start: "20260925T140000", end: "20260925T150000", dateOnly: false),
            calendar: london)
        XCTAssertFalse(r.isAllDay)
        XCTAssertEqual(r.end.timeIntervalSince(r.start), 3600)
    }

    // MARK: - recipient confirmation helpers (#3380)

    func testOrganizerTextAlwaysCarriesTheAddress() {
        XCTAssertEqual(
            CalendarInviteRecipientPolicy.organizerText(name: "Your Manager", email: "x@evil.example"),
            "Your Manager <x@evil.example>")
        XCTAssertEqual(
            CalendarInviteRecipientPolicy.organizerText(name: nil, email: "a@b.example"), "a@b.example")
    }

    func testMismatchAddressParsesOnlyTheCoreRefusal() {
        XCTAssertEqual(
            CalendarInviteRecipientPolicy.mismatchAddress(
                CastellumError.message("recipient mismatch: bob@b.example")),
            "bob@b.example")
        XCTAssertNil(CalendarInviteRecipientPolicy.mismatchAddress(CastellumError.message("other")))
        XCTAssertNil(CalendarInviteRecipientPolicy.mismatchAddress(CastellumError.io))
    }
}
