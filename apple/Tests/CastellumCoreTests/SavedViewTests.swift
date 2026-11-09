// Castellum - saved Active Work views: Swift DTO decode + live FFI lifecycle.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class SavedViewTests: XCTestCase {
    private func tempProfile() -> URL {
        let url = TestScratch.root
            .appendingPathComponent("castellum-savedviews-test-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }

    private var decoder: JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }

    func testSavedViewDecodesSnakeCaseWithRelativeDueFilter() throws {
        let json = """
        {"id": "builtin:today", "name": "Today",
         "filter": {"states": ["needs_reply", "waiting"], "due": ["overdue", "today"]},
         "position": 0, "built_in": true, "hidden": false,
         "created_at": 100, "updated_at": 200}
        """
        let view = try decoder.decode(SavedView.self, from: Data(json.utf8))
        XCTAssertEqual(view.id, "builtin:today")
        XCTAssertTrue(view.builtIn)
        XCTAssertFalse(view.hidden)
        XCTAssertEqual(view.filter.due, ["overdue", "today"])
        XCTAssertEqual(view.filter.states, ["needs_reply", "waiting"])
        XCTAssertNil(view.filter.waitingOnMe)
    }

    func testShorthandFilterCountAndUnassignedPageDecode() throws {
        let view = try decoder.decode(SavedView.self, from: Data("""
        {"id": "v1", "name": "Chasers", "filter": {"waiting_on_others": true},
         "position": 4, "built_in": false, "hidden": true,
         "created_at": 1, "updated_at": 2}
        """.utf8))
        XCTAssertEqual(view.filter.waitingOnOthers, true)
        XCTAssertTrue(view.hidden)

        let counts = try decoder.decode([SavedViewCount].self, from: Data(
            #"[{"id":"builtin:today","count":3},{"id":"v1","count":0}]"#.utf8))
        XCTAssertEqual(counts.map(\.count), [3, 0])

        let page = try decoder.decode(UnassignedMailPage.self, from: Data("""
        {"threads": [{"thread_id": "t1", "message_count": 2, "last_message_at": 350},
                     {"thread_id": "t2", "message_count": 1, "last_message_at": null}],
         "total": 2, "limit": 50, "offset": 0}
        """.utf8))
        XCTAssertEqual(page.threads.map(\.id), ["t1", "t2"])
        XCTAssertEqual(page.threads[0].lastMessageAt, 350)
        XCTAssertNil(page.threads[1].lastMessageAt)
        XCTAssertEqual(page.total, 2)
    }

    func testSavedViewLifecycleOverFFI() throws {
        let core = try CastellumCore(profilePath: tempProfile())

        // Built-ins are seeded once and in order.
        let views = try core.listSavedViews()
        XCTAssertEqual(views.map(\.name),
                       ["Today", "Needs reply", "Waiting on others", "Someday"])
        XCTAssertTrue(views.allSatisfy(\.builtIn))
        XCTAssertEqual(try core.listSavedViews().count, 4, "seeding is idempotent")

        // Custom create / update / reorder / delete; bad filters are refused.
        XCTAssertThrowsError(try core.createSavedView(name: "Bad", filterJSON: #"{"nope":1}"#))
        XCTAssertThrowsError(try core.createSavedView(name: "Bad", filterJSON: "not json"))
        let custom = try core.createSavedView(name: "Blocked",
                                              filterJSON: #"{"states":["blocked"]}"#)
        XCTAssertFalse(custom.builtIn)
        XCTAssertEqual(custom.filter.states, ["blocked"])
        let renamed = try core.updateSavedView(id: custom.id, name: "Stuck")
        XCTAssertEqual(renamed.name, "Stuck")
        XCTAssertThrowsError(try core.updateSavedView(id: "builtin:today", name: "Now"))
        let hiddenToday = try core.updateSavedView(id: "builtin:today", hidden: true)
        XCTAssertTrue(hiddenToday.hidden)
        XCTAssertEqual(try core.listSavedViews().count, 4, "hidden views leave the list")
        XCTAssertEqual(try core.listSavedViews(includeHidden: true).count, 5)

        let reordered = try core.reorderSavedViews(ids: [custom.id])
        XCTAssertEqual(reordered.first?.id, custom.id)

        // One call returns a count per visible view, matching the item list.
        _ = try core.upsertWorkItem(requestJSON: #"{"title":"A","state":"blocked"}"#)
        _ = try core.upsertWorkItem(requestJSON: #"{"title":"B","state":"waiting"}"#)
        let counts = try core.savedViewCounts()
        XCTAssertEqual(counts.count, 4)
        XCTAssertEqual(counts.first { $0.id == custom.id }?.count, 1)
        XCTAssertEqual(counts.first { $0.id == "builtin:waiting_on_others" }?.count, 1)
        XCTAssertNil(counts.first { $0.id == "builtin:today" }, "hidden views are not counted")
        XCTAssertEqual(try core.savedViewItems(id: custom.id).map(\.title), ["A"])

        XCTAssertThrowsError(try core.deleteSavedView(id: "builtin:someday"))
        try core.deleteSavedView(id: custom.id)
        XCTAssertEqual(try core.listSavedViews(includeHidden: true).count, 4)

        // Empty profile: no unassigned mail.
        let page = try core.listUnassignedMail()
        XCTAssertTrue(page.threads.isEmpty)
        XCTAssertEqual(page.limit, 50)
    }
}
