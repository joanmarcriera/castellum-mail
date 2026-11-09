// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Proves the Swift compose DTOs match the Rust core's JSON shapes exactly
/// (snake_case, optionality) — decoded with the same strategy CastellumCore uses.
final class ComposeDTOTests: XCTestCase {
    private let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return e
    }()

    func testDraftAttachmentMetaDecodes() throws {
        let json = """
        [{"id":1,"ordinal":0,"filename":"report.pdf","content_type":"application/pdf",
        "size_bytes":1234,"is_inline":false,"content_id":null}]
        """
        let list = try decoder.decode([DraftAttachmentMeta].self, from: Data(json.utf8))
        XCTAssertEqual(list.count, 1)
        XCTAssertEqual(list[0].filename, "report.pdf")
        XCTAssertEqual(list[0].sizeBytes, 1234)
        XCTAssertFalse(list[0].isInline)
    }

    func testNewDraftAttachmentEncodesSnakeCase() throws {
        let attachment = NewDraftAttachment(
            filename: "logo.png", contentType: "image/png",
            isInline: true, contentId: "cid1", dataBase64: "aGVsbG8=")
        let json = String(data: try encoder.encode(attachment), encoding: .utf8)!
        XCTAssertTrue(json.contains("\"content_type\""))
        XCTAssertTrue(json.contains("\"data_base64\""))
        XCTAssertTrue(json.contains("\"is_inline\""))
    }

    func testDraftDecodesFromRustShapedJSON() throws {
        // Field-for-field the serialization of castellum_models::Draft.
        let json = """
        {"id":7,"account_id":"a@x.test",
         "to":[{"name":"Ada","email":"ada@x.test"},{"name":null,"email":"bob@x.test"}],
         "cc":[],"bcc":[{"email":"secret@x.test"}],
         "subject":"Hello","body_text":"Line 1\\nLine 2",
         "in_reply_to":"<orig@x.test>","references_header":"<orig@x.test>",
         "status":"draft","updated_at_epoch":1751400000}
        """
        let d = try decoder.decode(Draft.self, from: Data(json.utf8))
        XCTAssertEqual(d.id, 7)
        XCTAssertEqual(d.accountId, "a@x.test")
        XCTAssertEqual(d.to, [DraftAddress(name: "Ada", email: "ada@x.test"),
                              DraftAddress(name: nil, email: "bob@x.test")])
        XCTAssertEqual(d.cc, [])
        XCTAssertEqual(d.bcc, [DraftAddress(name: nil, email: "secret@x.test")])
        XCTAssertEqual(d.subject, "Hello")
        XCTAssertEqual(d.bodyText, "Line 1\nLine 2")
        XCTAssertEqual(d.inReplyTo, "<orig@x.test>")
        XCTAssertEqual(d.status, "draft")
        XCTAssertEqual(d.updatedAtEpoch, 1_751_400_000)
    }

    func testSendReportDecodesFromRustShapedJSON() throws {
        let json = #"{"account_id":"a@x.test","sent":2,"failed":1}"#
        let r = try decoder.decode(SendReport.self, from: Data(json.utf8))
        XCTAssertEqual(r, SendReport(accountId: "a@x.test", sent: 2, failed: 1))
    }

    func testNewDraftEncodesSnakeCaseAndOmitsNilId() throws {
        let draft = NewDraft(accountId: "a@x.test",
                             to: [DraftAddress(name: "Ada", email: "ada@x.test")],
                             subject: "Hi", bodyText: "Body")
        let json = String(decoding: try encoder.encode(draft), as: UTF8.self)
        let obj = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        XCTAssertNil(obj["id"], "nil id must be omitted so the core inserts")
        XCTAssertEqual(obj["account_id"] as? String, "a@x.test")
        XCTAssertEqual(obj["body_text"] as? String, "Body")
        let to = try XCTUnwrap(obj["to"] as? [[String: Any]])
        XCTAssertEqual(to.first?["email"] as? String, "ada@x.test")
        XCTAssertNil(obj["in_reply_to"], "nil reply headers must be omitted")
    }

    func testNewDraftEncodesIdForUpdate() throws {
        var draft = NewDraft(accountId: "a@x.test", to: [])
        draft.id = 42
        draft.inReplyTo = "<orig@x.test>"
        draft.referencesHeader = "<orig@x.test>"
        let json = String(decoding: try encoder.encode(draft), as: UTF8.self)
        let obj = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        XCTAssertEqual(obj["id"] as? Int, 42)
        XCTAssertEqual(obj["in_reply_to"] as? String, "<orig@x.test>")
        XCTAssertEqual(obj["references_header"] as? String, "<orig@x.test>")
    }
}
