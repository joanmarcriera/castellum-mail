// Castellum - hydrating a message body must keep the thread id and calendar
// invite (defect hydrated-detail-rebuild-drops-invite-and-threadid, FP30).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class HydratedDetailStateTests: XCTestCase {
    func testHydratedDetailKeepsInviteAndThreadId() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-hydrate-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }
        XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)

        let header = MessageHeader(
            id: "a:INBOX:1", accountId: "a", mailboxId: "a:INBOX", subject: "Invite", fromName: nil,
            fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false, hasBody: true,
            isRead: true, isStarred: false, threadCount: 1)
        let invite = CalendarInvite(
            uid: "u1", sequence: 0, method: "REQUEST", summary: "Lunch", dtstart: nil, dtend: nil,
            organizerEmail: "o@example.test", organizerName: nil, attendees: [], ourResponse: nil, respondedAt: nil)
        let hydrated = MessageDetail(
            header: header, rfc822MessageId: nil, addresses: [], textBody: "hi", sanitizedHtml: nil,
            threadId: "thread-1", calendarInvite: invite)
        model.selectedMessage = header

        let refreshed = model.detailWithCurrentSelectionState(hydrated)

        XCTAssertEqual(refreshed.threadId, "thread-1")
        XCTAssertEqual(refreshed.calendarInvite, invite)
    }

    /// #3041: the core now sends LOCATION and DESCRIPTION; the model decodes them
    /// (and still decodes an older payload that lacks them).
    func testCalendarInviteDecodesLocationAndDescription() throws {
        let base = #"{"uid":"u","sequence":0,"method":"REQUEST","summary":"S","dtstart":null,"dtend":null,"organizer_email":null,"organizer_name":null,"attendees":[],"our_response":null,"responded_at":null"#
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let withText = try decoder.decode(CalendarInvite.self, from: Data((base + #","location":"Room 4","description":"Agenda"}"#).utf8))
        XCTAssertEqual(withText.location, "Room 4")
        XCTAssertEqual(withText.description, "Agenda")
        let legacy = try decoder.decode(CalendarInvite.self, from: Data((base + "}").utf8))
        XCTAssertNil(legacy.location)
    }
}
