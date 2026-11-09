// Castellum - the attachment strip does not list an invite twice (RP07).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AttachmentStripInviteTests: XCTestCase {
    private func detail(withInvite: Bool, attachments: String) throws -> MessageDetail {
        let invite = withInvite
            ? #","calendar_invite":{"uid":"e@x.com","sequence":0,"method":"REQUEST","summary":null,"dtstart":null,"dtend":null,"organizer_email":null,"organizer_name":null,"attendees":[],"our_response":null,"responded_at":null}"#
            : ""
        let json = """
        {"header":{"id":"m1","account_id":"a","mailbox_id":"a:INBOX","subject":null,
         "from_name":null,"from_address":null,"sent_at":null,"snippet":null,
         "has_attachments":true,"has_body":true,"is_read":false,"is_starred":false,
         "thread_count":1},
         "rfc822_message_id":null,"addresses":[],"text_body":null,"sanitized_html":null,
         "list_unsubscribe_one_click":false,"attachments":\(attachments)\(invite)}
        """
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(MessageDetail.self, from: Data(json.utf8))
    }

    private let both = """
    [{"ordinal":1,"filename":null,"content_type":"text/calendar","size_bytes":427,"section":"1.2","content_id":null,"is_inline":false,"transfer_encoding":null},
     {"ordinal":2,"filename":"invite.ics","content_type":"application/ics","size_bytes":427,"section":"2","content_id":null,"is_inline":false,"transfer_encoding":null}]
    """

    func testNamelessCalendarAlternativeIsHiddenWhenTheInviteCardShows() throws {
        let d = try detail(withInvite: true, attachments: both)
        XCTAssertEqual(d.stripAttachments.map(\.filename), ["invite.ics"])
    }

    func testNothingIsHiddenWithoutAnInvite() throws {
        let d = try detail(withInvite: false, attachments: both)
        XCTAssertEqual(d.stripAttachments.count, 2)
    }
}
