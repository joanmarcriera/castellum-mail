// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Proves the Swift project DTOs match the Rust core's JSON shapes exactly
/// (snake_case, optionality) — decoded with the same strategy CastellumCore uses.
final class ProjectDTOTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(T.self, from: Data(json.utf8))
    }

    // MARK: - ProjectCard

    func testProjectCardDecodesFromRustShapedJSON() throws {
        let json = """
        {"id":"proj-1","name":"Landing Page","notes":"MVP scope",
         "status":"active","color":"#3b82f6","position":0,
         "archived":false,"created_at":1751000000,"updated_at":1751100000,
         "last_activity_at":1751200000,"email_count":5,"contact_count":2}
        """
        let c = try decode(ProjectCard.self, json)
        XCTAssertEqual(c.id, "proj-1")
        XCTAssertEqual(c.name, "Landing Page")
        XCTAssertEqual(c.notes, "MVP scope")
        XCTAssertEqual(c.status, "active")
        XCTAssertEqual(c.color, "#3b82f6")
        XCTAssertEqual(c.position, 0)
        XCTAssertFalse(c.archived)
        XCTAssertEqual(c.createdAt, 1_751_000_000)
        XCTAssertEqual(c.updatedAt, 1_751_100_000)
        XCTAssertEqual(c.lastActivityAt, 1_751_200_000)
        XCTAssertEqual(c.emailCount, 5)
        XCTAssertEqual(c.contactCount, 2)
    }

    func testProjectCardNullableColorAndArchivedTrue() throws {
        let json = """
        {"id":"proj-2","name":"Old idea","notes":"","status":"someday",
         "color":null,"position":3,"archived":true,
         "created_at":1750000000,"updated_at":1750000000,
         "last_activity_at":1750000000,"email_count":0,"contact_count":0}
        """
        let c = try decode(ProjectCard.self, json)
        XCTAssertNil(c.color)
        XCTAssertTrue(c.archived)
        XCTAssertEqual(c.status, "someday")
    }

    func testProjectCardArrayAndEmpty() throws {
        let arr = try decode([ProjectCard].self, "[]")
        XCTAssertTrue(arr.isEmpty)
    }

    // MARK: - Project

    func testProjectDecodesFromRustShapedJSON() throws {
        let json = """
        {"id":"proj-3","name":"Blog","notes":"Write weekly",
         "status":"waiting","color":"#ef4444","position":1,
         "archived":false,"created_at":1751000000,"updated_at":1751050000,
         "last_activity_at":1751050000}
        """
        let p = try decode(Project.self, json)
        XCTAssertEqual(p.id, "proj-3")
        XCTAssertEqual(p.name, "Blog")
        XCTAssertEqual(p.status, "waiting")
        XCTAssertEqual(p.color, "#ef4444")
        XCTAssertFalse(p.archived)
    }

    // MARK: - ProjectContactLink

    func testProjectContactLinkDecodesFromRustShapedJSON() throws {
        let json = """
        {"contact_id":7,"email":"bob@example.com","derived_name":"Bob",
         "display_name":"Bob (client)","origin":"manual","linked_at":1751300000}
        """
        let link = try decode(ProjectContactLink.self, json)
        XCTAssertEqual(link.contactId, 7)
        XCTAssertEqual(link.email, "bob@example.com")
        XCTAssertEqual(link.derivedName, "Bob")
        XCTAssertEqual(link.displayName, "Bob (client)")
        XCTAssertEqual(link.origin, "manual")
        XCTAssertEqual(link.linkedAt, 1_751_300_000)
    }

    func testProjectContactLinkNullableNames() throws {
        let json = """
        {"contact_id":8,"email":"anon@example.com","derived_name":null,
         "display_name":null,"origin":"auto","linked_at":1751400000}
        """
        let link = try decode(ProjectContactLink.self, json)
        XCTAssertNil(link.derivedName)
        XCTAssertNil(link.displayName)
        XCTAssertEqual(link.origin, "auto")
    }

    // MARK: - ProjectDetail

    func testProjectDetailDecodesFromRustShapedJSON() throws {
        let json = """
        {"project":{"id":"proj-4","name":"Website","notes":"","status":"active",
          "color":null,"position":0,"archived":false,
          "created_at":1751000000,"updated_at":1751000000,"last_activity_at":1751000000},
         "emails":[{"id":"msg-1","account_id":"acc-1","mailbox_id":"mbox-1",
           "subject":"Hello","from_name":"Alice","from_address":"alice@x.test",
           "sent_at":1751100000,"snippet":"Hi there","has_attachments":false,
           "is_read":true,"is_starred":false,"thread_count":1}],
         "contacts":[{"contact_id":5,"email":"alice@x.test","derived_name":"Alice",
           "display_name":null,"origin":"auto","linked_at":1751100000}]}
        """
        let d = try decode(ProjectDetail.self, json)
        XCTAssertEqual(d.project.id, "proj-4")
        XCTAssertEqual(d.emails.count, 1)
        XCTAssertEqual(d.emails[0].subject, "Hello")
        XCTAssertEqual(d.contacts.count, 1)
        XCTAssertEqual(d.contacts[0].contactId, 5)
    }

    func testProjectDetailEmptyCollections() throws {
        let json = """
        {"project":{"id":"proj-5","name":"Empty","notes":"","status":"done",
          "color":null,"position":0,"archived":false,
          "created_at":1751000000,"updated_at":1751000000,"last_activity_at":1751000000},
         "emails":[],"contacts":[]}
        """
        let d = try decode(ProjectDetail.self, json)
        XCTAssertTrue(d.emails.isEmpty)
        XCTAssertTrue(d.contacts.isEmpty)
    }

    // MARK: - ProjectTag

    func testProjectTagDecodesFromRustShapedJSON() throws {
        let json = """
        [{"project_id":"proj-1","name":"Landing Page","color":"#3b82f6"},
         {"project_id":"proj-3","name":"Blog","color":null}]
        """
        let tags = try decode([ProjectTag].self, json)
        XCTAssertEqual(tags.count, 2)
        XCTAssertEqual(tags[0].projectId, "proj-1")
        XCTAssertEqual(tags[0].name, "Landing Page")
        XCTAssertEqual(tags[0].color, "#3b82f6")
        XCTAssertNil(tags[1].color)
    }

    func testProjectTagEmptyArray() throws {
        let tags = try decode([ProjectTag].self, "[]")
        XCTAssertTrue(tags.isEmpty)
    }
}
