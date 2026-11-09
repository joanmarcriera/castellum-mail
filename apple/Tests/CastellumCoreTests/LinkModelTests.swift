// Castellum - shared link model: Swift DTO decode + live FFI round trip.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class LinkModelTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-links-test-\(UUID().uuidString)")
    }

    func testLinkRecordDecodesSnakeCase() throws {
        let json = """
        {"id": 3, "src_kind": "project", "src_id": "p1",
         "dst_kind": "contact", "dst_id": "42", "relation": "involves",
         "origin": "derived", "provenance": "message:acc:INBOX:9",
         "confidence": 0.75, "note": "met at FOSDEM",
         "created_at": 100, "updated_at": 200}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let record = try decoder.decode(LinkRecord.self, from: Data(json.utf8))
        XCTAssertEqual(record.srcKind, "project")
        XCTAssertEqual(record.dstId, "42")
        XCTAssertEqual(record.origin, "derived")
        XCTAssertEqual(record.provenance, "message:acc:INBOX:9")
        XCTAssertEqual(record.confidence, 0.75)
        XCTAssertEqual(record.note, "met at FOSDEM")
        XCTAssertEqual(record.updatedAt, 200)
    }

    func testLinkLifecycleOverFFI() throws {
        let core = try CastellumCore(profilePath: tempProfile())

        // Endpoint existence is enforced on write, so link real rows: a project
        // and a hand-created contact.
        let project = try core.upsertProject(json: #"{"name":"P","status":"active"}"#)
        let contact = try core.createContact(email: "c@x.test", name: nil)
        let pid = project.id
        let cid = String(contact.id)

        let stored = try core.upsertLink(requestJSON: """
        {"src_kind":"project","src_id":"\(pid)",
         "dst_kind":"contact","dst_id":"\(cid)","relation":"involves"}
        """)
        XCTAssertEqual(stored.origin, "manual", "origin defaults to manual")
        XCTAssertNil(stored.note)

        try core.setLinkNote(requestJSON: """
        {"src_kind":"project","src_id":"\(pid)",
         "dst_kind":"contact","dst_id":"\(cid)",
         "relation":"involves","note":"why: follow-up thread"}
        """)
        let links = try core.linksFor(kind: "project", id: pid)
        XCTAssertEqual(links.count, 1)
        XCTAssertEqual(links[0].note, "why: follow-up thread")

        try core.unlink(requestJSON: """
        {"src_kind":"project","src_id":"\(pid)",
         "dst_kind":"contact","dst_id":"\(cid)","relation":"involves"}
        """)
        XCTAssertTrue(try core.linksFor(kind: "project", id: pid).isEmpty)

        // Nonexistent endpoints are rejected on write.
        XCTAssertThrowsError(try core.upsertLink(requestJSON: """
        {"src_kind":"project","src_id":"\(pid)",
         "dst_kind":"message","dst_id":"acc:INBOX:404","relation":"includes"}
        """))
        // Unknown entity kinds surface as thrown errors, not silent drops.
        XCTAssertThrowsError(try core.linksFor(kind: "banana", id: "x"))
    }
}
