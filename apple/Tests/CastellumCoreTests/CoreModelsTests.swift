// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

final class CoreModelsTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(T.self, from: Data(json.utf8))
    }

    func testMessageHeaderDecodesSnakeCase() throws {
        let json = """
        {"id":"a:INBOX:1","account_id":"a","mailbox_id":"a:INBOX","subject":"Hi",
         "from_name":"Alice","from_address":"alice@x","sent_at":1720000000,
         "snippet":"hello","has_attachments":true,"has_body":true,
         "is_read":false,"is_starred":true,"thread_count":3}
        """
        let h = try decode(MessageHeader.self, json)
        XCTAssertEqual(h.id, "a:INBOX:1")
        XCTAssertEqual(h.fromAddress, "alice@x")
        XCTAssertEqual(h.sentAt, 1_720_000_000)
        XCTAssertTrue(h.hasAttachments)
        XCTAssertEqual(h.hasBody, true)
        XCTAssertFalse(h.isRead)
        XCTAssertTrue(h.isStarred)
        XCTAssertEqual(h.threadCount, 3)
    }

    func testMailboxCountsDecode() throws {
        let arr = try decode([MailboxCounts].self,
            #"[{"mailbox_id":"a:INBOX","total":10,"unread":3}]"#)
        XCTAssertEqual(arr.first?.total, 10)
        XCTAssertEqual(arr.first?.unread, 3)
        XCTAssertEqual(arr.first?.id, "a:INBOX")
    }

    func testOperationRecordDecode() throws {
        let arr = try decode([OperationRecord].self,
            #"[{"id":5,"account_id":"a","kind":"mark_read","payload_json":"{}","status":"queued","attempts":0,"last_error":null}]"#)
        XCTAssertEqual(arr.first?.id, 5)
        XCTAssertEqual(arr.first?.kind, "mark_read")
        XCTAssertEqual(arr.first?.status, "queued")
        XCTAssertNil(arr.first?.lastError)
    }

    func testMessageDetailAndNullsDecode() throws {
        let json = """
        {"header":{"id":"a:INBOX:1","account_id":"a","mailbox_id":"a:INBOX",
         "subject":null,"from_name":null,"from_address":null,"sent_at":null,
         "snippet":null,"has_attachments":false,"is_read":true,"is_starred":false,"thread_count":1},
         "rfc822_message_id":"m@x","addresses":[{"kind":"to","name":null,"address":"b@x"}],
         "text_body":"body","sanitized_html":null,"list_unsubscribe_one_click":false}
        """
        let d = try decode(MessageDetail.self, json)
        XCTAssertNil(d.header.subject)
        XCTAssertEqual(d.addresses.first?.kind, "to")
        XCTAssertEqual(d.textBody, "body")
    }

    func testMessageDetailDecodesAttachments() throws {
        let json = """
        {"header":{"id":"m1","account_id":"a","mailbox_id":"mb","subject":"s",
        "from_name":null,"from_address":null,"sent_at":null,"snippet":null,
        "has_attachments":true,"has_body":true,"is_read":false,"is_starred":false,
        "thread_count":1},
        "rfc822_message_id":null,"addresses":[],"text_body":"body",
        "sanitized_html":null,"has_remote_content":false,
        "attachments":[{"ordinal":0,"filename":"report.pdf",
        "content_type":"application/pdf","size_bytes":1234,"section":"3",
        "content_id":null,"is_inline":false,"transfer_encoding":"base64"}],
        "list_unsubscribe_one_click":false}
        """
        let detail = try decode(MessageDetail.self, json)
        XCTAssertEqual(detail.attachments?.count, 1)
        XCTAssertEqual(detail.attachments?.first?.filename, "report.pdf")
        XCTAssertEqual(detail.attachments?.first?.section, "3")
        XCTAssertEqual(detail.attachments?.first?.transferEncoding, "base64")
    }

    func testMessageDetailDecodesTrustSignals() throws {
        let json = """
        {"header":{"id":"m1","account_id":"a","mailbox_id":"b","subject":null,"from_name":null,
         "from_address":null,"sent_at":null,"snippet":null,"has_attachments":false,"has_body":true,
         "is_read":false,"is_starred":false,"thread_count":1},
         "rfc822_message_id":null,"addresses":[],"text_body":"body","sanitized_html":null,
         "has_remote_content":false,"attachments":[],"list_unsubscribe_one_click":false,
         "trust":{"spf":"pass","dkim":"fail","dmarc":"none","authserv_id":"imap.fastmail.com"}}
        """
        let detail = try decode(MessageDetail.self, json)
        XCTAssertEqual(detail.trust?.spf, "pass")
        XCTAssertEqual(detail.trust?.dkim, "fail")
        XCTAssertEqual(detail.trust?.authservId, "imap.fastmail.com")
    }

    func testMessageDetailDecodesWithoutTrustKeyForOlderCorePayloads() throws {
        let json = """
        {"header":{"id":"m1","account_id":"a","mailbox_id":"b","subject":null,"from_name":null,
         "from_address":null,"sent_at":null,"snippet":null,"has_attachments":false,"has_body":true,
         "is_read":false,"is_starred":false,"thread_count":1},
         "rfc822_message_id":null,"addresses":[],"text_body":"body","sanitized_html":null,
         "has_remote_content":false,"attachments":[],"list_unsubscribe_one_click":false}
        """
        let detail = try decode(MessageDetail.self, json)
        XCTAssertNil(detail.trust)
    }

    /// Regression test for the S44 review finding: a stored `let` property
    /// declared with an inline default (`let x: T = v`) is silently excluded
    /// from Swift's synthesized `Decodable.init(from:)` — the JSON key is
    /// never read even when present. `MessageDetail` moved those defaults
    /// into its custom memberwise init instead; this proves the 3
    /// List-Unsubscribe fields now decode real values from JSON.
    func testMessageDetailDecodesListUnsubscribeFields() throws {
        let json = """
        {"header":{"id":"a:INBOX:1","account_id":"a","mailbox_id":"a:INBOX",
         "subject":"Newsletter","from_name":null,"from_address":null,"sent_at":null,
         "snippet":null,"has_attachments":false,"is_read":false,"is_starred":false,"thread_count":1},
         "rfc822_message_id":null,"addresses":[],"text_body":"body","sanitized_html":null,
         "list_unsubscribe_mailto":"mailto:unsub@example.com?subject=unsubscribe",
         "list_unsubscribe_url":"https://example.com/unsub?id=42",
         "list_unsubscribe_one_click":true}
        """
        let detail = try decode(MessageDetail.self, json)
        XCTAssertEqual(detail.listUnsubscribeMailto, "mailto:unsub@example.com?subject=unsubscribe")
        XCTAssertEqual(detail.listUnsubscribeUrl, "https://example.com/unsub?id=42")
        XCTAssertTrue(detail.listUnsubscribeOneClick)
    }

    func testMessageDetailWithNoUnsubscribeTargetsDecodesAsAbsent() throws {
        let json = """
        {"header":{"id":"a:INBOX:2","account_id":"a","mailbox_id":"a:INBOX",
         "subject":"Plain","from_name":null,"from_address":null,"sent_at":null,
         "snippet":null,"has_attachments":false,"is_read":false,"is_starred":false,"thread_count":1},
         "rfc822_message_id":null,"addresses":[],"text_body":"body","sanitized_html":null,
         "list_unsubscribe_mailto":null,"list_unsubscribe_url":null,
         "list_unsubscribe_one_click":false}
        """
        let detail = try decode(MessageDetail.self, json)
        XCTAssertNil(detail.listUnsubscribeMailto)
        XCTAssertNil(detail.listUnsubscribeUrl)
        XCTAssertFalse(detail.listUnsubscribeOneClick)
    }

    func testThreadSummaryListDecodes() throws {
        let arr = try decode([ThreadSummary].self,
            #"[{"id":"t:1","account_id":"a","subject":"S","last_message_at":10,"message_count":3,"unread_count":1}]"#)
        XCTAssertEqual(arr.first?.messageCount, 3)
        XCTAssertEqual(arr.first?.unreadCount, 1)
    }

    func testSyncReportDecodes() throws {
        let r = try decode(SyncReport.self,
            #"{"account_id":"gm","mailboxes":[{"path":"INBOX","fetched":14,"descriptors_fetched":14,"bodies_fetched":4,"bytes_fetched":2048,"fetched_uid_min":7,"fetched_uid_max":20,"total":14}]}"#)
        XCTAssertEqual(r.accountId, "gm")
        XCTAssertEqual(r.mailboxes.first?.total, 14)
        XCTAssertEqual(r.mailboxes.first?.descriptorsFetched, 14)
        XCTAssertEqual(r.mailboxes.first?.bodiesFetched, 4)
        XCTAssertEqual(r.mailboxes.first?.bytesFetched, 2048)
        XCTAssertEqual(r.mailboxes.first?.fetchedUidMin, 7)
        XCTAssertEqual(r.mailboxes.first?.fetchedUidMax, 20)
    }

    func testHydrationReportDecodes() throws {
        let r = try decode(HydrationReport.self,
            #"{"account_id":"gm","mailboxes":[{"path":"INBOX","requested":25,"hydrated":24,"bytes_fetched":4096,"duration_ms":120,"remaining":35,"error":null},{"path":"Archive","requested":10,"hydrated":0,"bytes_fetched":0,"duration_ms":5,"remaining":10,"error":"body fetch failed"}],"requested":35,"hydrated":24,"bytes_fetched":4096,"remaining":45,"elapsed_ms":140}"#)
        XCTAssertEqual(r.accountId, "gm")
        XCTAssertEqual(r.requested, 35)
        XCTAssertEqual(r.hydrated, 24)
        XCTAssertEqual(r.bytesFetched, 4096)
        XCTAssertEqual(r.remaining, 45)
        XCTAssertEqual(r.elapsedMs, 140)
        XCTAssertEqual(r.mailboxes.count, 2)
        XCTAssertEqual(r.mailboxes.first?.path, "INBOX")
        XCTAssertEqual(r.mailboxes.first?.remaining, 35)
        XCTAssertNil(r.mailboxes.first?.error)
        XCTAssertEqual(r.mailboxes.last?.error, "body fetch failed")

        // Rust omits the error key entirely (not null) when a mailbox succeeds.
        let omitted = try decode(HydrationReport.self,
            #"{"account_id":"gm","mailboxes":[{"path":"INBOX","requested":0,"hydrated":0,"bytes_fetched":0,"duration_ms":1,"remaining":0}],"requested":0,"hydrated":0,"bytes_fetched":0,"remaining":0,"elapsed_ms":1}"#)
        XCTAssertNil(omitted.mailboxes.first?.error)
    }

    // MARK: - SyncProgress

    func testSyncProgressDecodesMidMailbox() throws {
        let r = try decode(SyncProgress.self,
            #"{"account_id":"gm","mailboxes_completed":1,"mailboxes_total":6,"current_mailbox":{"path":"INBOX","fetched":1204,"total":12830},"aggregate_fetched":1204,"aggregate_total":12830,"done":false}"#)
        XCTAssertEqual(r.accountId, "gm")
        XCTAssertEqual(r.mailboxesCompleted, 1)
        XCTAssertEqual(r.mailboxesTotal, 6)
        XCTAssertEqual(r.currentMailbox?.path, "INBOX")
        XCTAssertEqual(r.currentMailbox?.fetched, 1204)
        XCTAssertEqual(r.currentMailbox?.total, 12830)
        XCTAssertEqual(r.aggregateFetched, 1204)
        XCTAssertEqual(r.aggregateTotal, 12830)
        XCTAssertFalse(r.done)
    }

    func testSyncProgressDecodesWithCurrentMailboxOmittedBetweenMailboxes() throws {
        // Rust omits `current_mailbox` entirely (not null) once a mailbox
        // finishes and before the next one is selected, or once `done`.
        let r = try decode(SyncProgress.self,
            #"{"account_id":"gm","mailboxes_completed":6,"mailboxes_total":6,"aggregate_fetched":78851,"aggregate_total":78851,"done":true}"#)
        XCTAssertNil(r.currentMailbox)
        XCTAssertTrue(r.done)
    }

    func testSyncProgressAsOptionalDecodesNullAsNoSnapshotYet() throws {
        // `CastellumCore.syncProgress` decodes `SyncProgress?`: the FFI
        // returns the JSON `null` literal when no sync has ever run for the
        // account id this process, which must decode to `nil`, not throw.
        let r = try decode(SyncProgress?.self, "null")
        XCTAssertNil(r)
    }

    func testMissingMessageDecodesAsNil() throws {
        let d = try decode(MessageDetail?.self, "null")
        XCTAssertNil(d)
    }

    func testReadingPaneBodyStateForDescriptorOnlyRows() {
        let descriptor = makeDetail(hasBody: false, hasAttachments: true)

        XCTAssertEqual(
            ReadingPaneBodyState.evaluate(detail: descriptor, isHydrating: true, error: nil),
            .loading
        )
        XCTAssertEqual(
            ReadingPaneBodyState.evaluate(detail: descriptor, isHydrating: false, error: nil),
            .notDownloaded(hasAttachments: true)
        )
        XCTAssertEqual(
            ReadingPaneBodyState.evaluate(detail: descriptor, isHydrating: true, error: "offline"),
            .notDownloaded(hasAttachments: true)
        )
    }

    func testReadingPaneBodyStateTreatsHydratedAndLegacyRowsAsReady() {
        XCTAssertEqual(
            ReadingPaneBodyState.evaluate(
                detail: makeDetail(hasBody: true, hasAttachments: false),
                isHydrating: true,
                error: nil
            ),
            .ready
        )
        XCTAssertEqual(
            ReadingPaneBodyState.evaluate(
                detail: makeDetail(hasBody: nil, hasAttachments: false),
                isHydrating: true,
                error: nil
            ),
            .ready
        )
    }

    func testBodyDependentActionsRequireHydratedOrLegacyDetail() {
        XCTAssertFalse(messageBodyActionsAvailable(nil))
        XCTAssertFalse(messageBodyActionsAvailable(
            makeDetail(hasBody: false, hasAttachments: true)
        ))
        XCTAssertTrue(messageBodyActionsAvailable(
            makeDetail(hasBody: true, hasAttachments: false)
        ))
        XCTAssertTrue(messageBodyActionsAvailable(
            makeDetail(hasBody: nil, hasAttachments: false)
        ))
    }

    func testRelativeDate() {
        let now = Date(timeIntervalSince1970: 1_720_000_000)
        XCTAssertFalse(relativeDate(1_720_000_000, now: now).isEmpty) // same day -> time
        XCTAssertEqual(relativeDate(nil), "")
    }

    private func makeDetail(hasBody: Bool?, hasAttachments: Bool) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "a:INBOX:1",
                accountId: "a",
                mailboxId: "a:INBOX",
                subject: "Hi",
                fromName: "Alice",
                fromAddress: "alice@example.com",
                sentAt: 1_720_000_000,
                snippet: nil,
                hasAttachments: hasAttachments,
                hasBody: hasBody,
                isRead: false,
                isStarred: false,
                threadCount: 1
            ),
            rfc822MessageId: nil,
            addresses: [],
            textBody: hasBody == false ? nil : "Body",
            sanitizedHtml: nil,
            hasRemoteContent: nil,
            attachments: nil,
            trust: nil
        )
    }

    // MARK: list cursor (#3043)

    func testDateCursorEncodesWithoutAKeyField() throws {
        let data = try CastellumCore.encoder.encode(ListCursor(sentAt: 5, id: "x"))
        let json = String(decoding: data, as: UTF8.self)
        XCTAssertFalse(json.contains("key"), json)
    }

    func testTextSortCursorRoundTripsItsFoldedKey() throws {
        let wire = Data(#"{"sent_at":null,"id":"m1","key":"alex"}"#.utf8)
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let cursor = try decoder.decode(ListCursor.self, from: wire)
        XCTAssertEqual(cursor.key, "alex")
        XCTAssertNil(cursor.sentAt)
        let back = try CastellumCore.encoder.encode(cursor)
        XCTAssertTrue(String(decoding: back, as: UTF8.self).contains(#""key":"alex""#))
    }
}
