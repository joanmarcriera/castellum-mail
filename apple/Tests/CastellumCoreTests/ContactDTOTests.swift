// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Proves the Swift contact DTOs match the Rust core's JSON shapes exactly
/// (snake_case, optionality) — decoded with the same strategy CastellumCore uses.
final class ContactDTOTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(T.self, from: Data(json.utf8))
    }

    func testContactDecodesFromRustShapedJSON() throws {
        // Field-for-field the serialization of castellum_models::ContactRecord.
        let json = """
        {"id":3,"email":"alice@x.test","derived_name":"Alice Ada",
         "display_name":"Alice (client)","notes":"met at conf","is_hidden":false,
         "is_self":false,"is_user_created":false,
         "first_seen_at":1751000000,"last_seen_at":1751400000,
         "inbound_count":12,"outbound_count":4}
        """
        let c = try decode(Contact.self, json)
        XCTAssertEqual(c.id, 3)
        XCTAssertEqual(c.email, "alice@x.test")
        XCTAssertEqual(c.derivedName, "Alice Ada")
        XCTAssertEqual(c.displayName, "Alice (client)")
        XCTAssertEqual(c.notes, "met at conf")
        XCTAssertFalse(c.isHidden)
        XCTAssertFalse(c.isSelf)
        XCTAssertFalse(c.isUserCreated)
        XCTAssertEqual(c.firstSeenAt, 1_751_000_000)
        XCTAssertEqual(c.lastSeenAt, 1_751_400_000)
        XCTAssertEqual(c.inboundCount, 12)
        XCTAssertEqual(c.outboundCount, 4)
        XCTAssertEqual(c.bestName, "Alice (client)", "user override wins")
    }

    func testContactNullableFieldsAndBestNameFallbacks() throws {
        let json = """
        {"id":9,"email":"new@x.test","derived_name":null,"display_name":null,
         "notes":"","is_hidden":true,"is_self":true,"is_user_created":true,
         "first_seen_at":null,"last_seen_at":null,
         "inbound_count":0,"outbound_count":0}
        """
        let c = try decode(Contact.self, json)
        XCTAssertNil(c.derivedName)
        XCTAssertNil(c.displayName)
        XCTAssertNil(c.firstSeenAt)
        XCTAssertNil(c.lastSeenAt)
        XCTAssertTrue(c.isHidden)
        XCTAssertTrue(c.isSelf)
        XCTAssertTrue(c.isUserCreated)
        XCTAssertEqual(c.bestName, "new@x.test", "falls back to the address")

        // derived_name present, display_name absent -> derived wins.
        let json2 = """
        {"id":10,"email":"bob@x.test","derived_name":"Bob","display_name":null,
         "notes":"","is_hidden":false,"is_self":false,"is_user_created":false,
         "first_seen_at":null,"last_seen_at":null,
         "inbound_count":1,"outbound_count":0}
        """
        XCTAssertEqual(try decode(Contact.self, json2).bestName, "Bob")
    }

    func testContactArrayAndMissingContactDecode() throws {
        let arr = try decode([Contact].self, "[]")
        XCTAssertTrue(arr.isEmpty)
        let missing = try decode(Contact?.self, "null")
        XCTAssertNil(missing)
    }

    func testAggregateReportDecodesFromRustShapedJSON() throws {
        let r = try decode(AggregateReport.self,
            #"{"contacts_upserted":42,"edges_rebuilt":17,"elapsed_ms":8}"#)
        XCTAssertEqual(r.contactsUpserted, 42)
        XCTAssertEqual(r.edgesRebuilt, 17)
        XCTAssertEqual(r.elapsedMs, 8)
    }

    func testEdgeRecordDecodesFromRustShapedJSON() throws {
        let arr = try decode([EdgeRecord].self, """
        [{"contact_a":1,"contact_b":2,"weight":1.25,
          "message_count":3,"last_seen_at":1751400000}]
        """)
        XCTAssertEqual(arr.first?.contactA, 1)
        XCTAssertEqual(arr.first?.contactB, 2)
        XCTAssertEqual(arr.first?.weight ?? 0, 1.25, accuracy: 1e-9)
        XCTAssertEqual(arr.first?.messageCount, 3)
        XCTAssertEqual(arr.first?.lastSeenAt, 1_751_400_000)
    }

    // MARK: - Enrichment DTOs (slice 8d)

    func testEnrichmentFieldDecodesFromRustShape() throws {
        // Mirrors EnrichmentFieldOut serialized by the Rust core.
        let json = """
        {"id":7,"source":"manual","field":"title","value":"Senior Engineer",
         "provenance":"pasted 2026-07-03","fetched_at":1751500000}
        """
        let f = try decode(EnrichmentField.self, json)
        XCTAssertEqual(f.id, 7)
        XCTAssertEqual(f.source, "manual")
        XCTAssertEqual(f.field, "title")
        XCTAssertEqual(f.value, "Senior Engineer")
        XCTAssertEqual(f.provenance, "pasted 2026-07-03")
        XCTAssertEqual(f.fetchedAt, 1_751_500_000)
    }

    func testEnrichmentFieldNullableProvenance() throws {
        let json = """
        {"id":2,"source":"manual","field":"company","value":"Acme",
         "provenance":null,"fetched_at":1751500000}
        """
        let f = try decode(EnrichmentField.self, json)
        XCTAssertNil(f.provenance)
        XCTAssertEqual(f.value, "Acme")
    }

    func testPreviewEnrichmentFieldDecodesRealRustPreviewOutput() throws {
        // VERBATIM output of the Rust core for
        // enrich(save:false, input:"Eve Adams at Initech\nAustin, TX\n
        // https://linkedin.com/in/evadams\nLoves shipping.") captured over the
        // C ABI on 2026-07-03. The Rust FFI test
        // `enrichment_preview_and_save_shapes_over_the_c_abi` pins this shape.
        let json = """
        [{"field":"title","value":"Eve Adams","provenance":"pasted 2026-07-03"},\
        {"field":"company","value":"Initech","provenance":"pasted 2026-07-03"},\
        {"field":"location","value":"Austin, TX","provenance":"pasted 2026-07-03"},\
        {"field":"linkedin_url","value":"https://linkedin.com/in/evadams",\
        "provenance":"pasted 2026-07-03"},\
        {"field":"summary","value":"Loves shipping.","provenance":"pasted 2026-07-03"}]
        """
        let fields = try decode([PreviewEnrichmentField].self, json)
        XCTAssertEqual(fields.count, 5)
        XCTAssertEqual(fields[0].field, "title")
        XCTAssertEqual(fields[0].value, "Eve Adams")
        XCTAssertEqual(fields[0].provenance, "pasted 2026-07-03")
        XCTAssertEqual(fields[3].field, "linkedin_url")
        XCTAssertEqual(fields[3].value, "https://linkedin.com/in/evadams")
        XCTAssertEqual(fields[4].field, "summary")
    }

    func testEnrichmentFieldCannotDecodePreviewPayload() throws {
        // Regression guard for the preview-decode bug: preview rows lack
        // id/source/fetched_at, so decoding them as saved EnrichmentField MUST
        // fail — the UI has to use PreviewEnrichmentField for previews.
        let previewJSON = """
        [{"field":"title","value":"Eve Adams","provenance":"pasted 2026-07-03"}]
        """
        XCTAssertThrowsError(try decode([EnrichmentField].self, previewJSON)) { error in
            guard case DecodingError.keyNotFound = error else {
                return XCTFail("expected keyNotFound, got \(error)")
            }
        }
    }

    func testEnrichmentSourceInfoDecodes() throws {
        let json = """
        {"id":"manual","display_name":"Manual paste",
         "kind":"PasteImport","input_label":"Paste profile text or URL"}
        """
        let s = try decode(EnrichmentSourceInfo.self, json)
        XCTAssertEqual(s.id, "manual")
        XCTAssertEqual(s.displayName, "Manual paste")
        XCTAssertEqual(s.kind, "PasteImport")
        XCTAssertEqual(s.inputLabel, "Paste profile text or URL")
    }

    func testContactDetailDecodesWithEnrichment() throws {
        // get_contact now returns all ContactRecord fields + enrichment array.
        let json = """
        {"id":5,"email":"carol@x.test","derived_name":"Carol","display_name":null,
         "notes":"key client","is_hidden":false,"is_self":false,"is_user_created":false,
         "first_seen_at":1750000000,"last_seen_at":1751500000,
         "inbound_count":8,"outbound_count":2,
         "enrichment":[
           {"id":1,"source":"manual","field":"title","value":"VP Engineering",
            "provenance":"pasted 2026-07-03","fetched_at":1751500000},
           {"id":2,"source":"manual","field":"company","value":"TechCorp",
            "provenance":"pasted 2026-07-03","fetched_at":1751500000}
         ]}
        """
        let d = try decode(ContactDetail.self, json)
        XCTAssertEqual(d.id, 5)
        XCTAssertEqual(d.email, "carol@x.test")
        XCTAssertEqual(d.derivedName, "Carol")
        XCTAssertNil(d.displayName)
        XCTAssertEqual(d.notes, "key client")
        XCTAssertEqual(d.inboundCount, 8)
        XCTAssertEqual(d.enrichment.count, 2)
        XCTAssertEqual(d.enrichment[0].field, "title")
        XCTAssertEqual(d.enrichment[0].value, "VP Engineering")
        XCTAssertEqual(d.enrichment[1].field, "company")
        XCTAssertEqual(d.bestName, "Carol", "displayName nil → derivedName wins")
    }

    func testContactDetailDecodesWithEmptyEnrichment() throws {
        let json = """
        {"id":6,"email":"dave@x.test","derived_name":null,"display_name":"Dave",
         "notes":"","is_hidden":false,"is_self":false,"is_user_created":true,
         "first_seen_at":null,"last_seen_at":null,
         "inbound_count":0,"outbound_count":0,
         "enrichment":[]}
        """
        let d = try decode(ContactDetail.self, json)
        XCTAssertEqual(d.id, 6)
        XCTAssertTrue(d.enrichment.isEmpty)
        XCTAssertEqual(d.bestName, "Dave")
        // asContact conversion preserves all fields.
        let c = d.asContact
        XCTAssertEqual(c.id, d.id)
        XCTAssertEqual(c.email, d.email)
        XCTAssertEqual(c.displayName, d.displayName)
    }

    func testContactDetailNullableDecodes() throws {
        let missing = try decode(ContactDetail?.self, "null")
        XCTAssertNil(missing)
    }
}
