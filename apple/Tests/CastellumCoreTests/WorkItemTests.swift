// Castellum - work items: Swift DTO decode + live FFI lifecycle.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class WorkItemTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-workitems-test-\(UUID().uuidString)")
    }

    func testWorkItemDecodesSnakeCase() throws {
        let json = """
        {"id": "w1", "project_id": "p1", "title": "Chase reply",
         "notes": "asked twice", "state": "waiting", "position": 2,
         "created_at": 100, "updated_at": 200}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let item = try decoder.decode(WorkItem.self, from: Data(json.utf8))
        XCTAssertEqual(item.projectId, "p1")
        XCTAssertEqual(item.state, "waiting")
        XCTAssertEqual(item.position, 2)
    }

    func testWorkItemDecodesOldShapeWithoutTheAW1Fields() throws {
        // Pre-0045 JSON: no due/waiting/completed/delegate keys at all.
        let json = """
        {"id": "w1", "project_id": "p1", "title": "T", "notes": "",
         "state": "needs_reply", "position": 0, "created_at": 1, "updated_at": 2}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let item = try decoder.decode(WorkItem.self, from: Data(json.utf8))
        XCTAssertNil(item.dueAt)
        XCTAssertNil(item.waitingSince)
        XCTAssertNil(item.completedAt)
        XCTAssertNil(item.delegatedToContactId)
    }

    func testWorkItemDecodesNewShapeWithDatesDelegateAndNoProject() throws {
        let json = """
        {"id": "w2", "project_id": null, "title": "T", "notes": "n",
         "state": "delegated", "position": 1, "created_at": 1, "updated_at": 2,
         "due_at": 1800000000, "waiting_since": 1700000000,
         "completed_at": null, "delegated_to_contact_id": 7}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let item = try decoder.decode(WorkItem.self, from: Data(json.utf8))
        XCTAssertNil(item.projectId, "null project = unassigned")
        XCTAssertEqual(item.dueAt, 1_800_000_000)
        XCTAssertEqual(item.waitingSince, 1_700_000_000)
        XCTAssertNil(item.completedAt)
        XCTAssertEqual(item.delegatedToContactId, 7)
    }

    func testFilteredListAndReverseLookupOverFFI() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let project = try core.upsertProject(json: #"{"name":"P","status":"active"}"#)
        let late = try core.upsertWorkItem(requestJSON: """
        {"project_id":"\(project.id)","title":"Late","state":"waiting","due_at":100}
        """)
        XCTAssertNotNil(late.waitingSince, "core stamps waiting_since")
        XCTAssertEqual(late.dueAt, 100)
        _ = try core.upsertWorkItem(requestJSON: """
        {"project_id":"\(project.id)","title":"Undated"}
        """)
        let loose = try core.upsertWorkItem(requestJSON: #"{"title":"Loose","state":"someday"}"#)
        XCTAssertNil(loose.projectId)

        let ordered = try core.listWorkItemsFiltered(filterJSON: "{}")
        XCTAssertEqual(ordered.first?.title, "Late", "due items come first")
        XCTAssertEqual(Set(ordered.map(\.title)), ["Late", "Loose", "Undated"])
        XCTAssertEqual(
            try core.listWorkItemsFiltered(filterJSON: #"{"unassigned":true}"#).map(\.title),
            ["Loose"])
        XCTAssertEqual(
            try core.listWorkItemsFiltered(filterJSON: #"{"overdue":true}"#).map(\.title),
            ["Late"])
        XCTAssertThrowsError(try core.listWorkItemsFiltered(filterJSON: #"{"bogus":1}"#),
                             "unknown filter keys are rejected")
        XCTAssertTrue(try core.workItems(forKind: "thread", id: "no-such-thread").isEmpty)
        XCTAssertThrowsError(try core.workItems(forKind: "project", id: project.id))
    }

    func testWorkItemLifecycleOverFFI() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let project = try core.upsertProject(json: #"{"name":"P","status":"active"}"#)

        let created = try core.upsertWorkItem(requestJSON: """
        {"project_id":"\(project.id)","title":"Chase reply","state":"waiting"}
        """)
        XCTAssertEqual(created.state, "waiting")
        XCTAssertEqual(created.notes, "")

        let updated = try core.upsertWorkItem(requestJSON: """
        {"id":"\(created.id)","project_id":"\(project.id)",
         "title":"Chase reply","notes":"asked on Monday","state":"delegated"}
        """)
        XCTAssertEqual(updated.notes, "asked on Monday", "notes are user-owned")
        XCTAssertEqual(updated.createdAt, created.createdAt)

        XCTAssertEqual(try core.listWorkItems(projectId: project.id).count, 1)
        XCTAssertThrowsError(try core.upsertWorkItem(requestJSON: """
        {"project_id":"\(project.id)","title":"T","state":"doing"}
        """), "invalid states are rejected")

        try core.deleteWorkItem(id: created.id)
        XCTAssertTrue(try core.listWorkItems(projectId: project.id).isEmpty)
    }
}
