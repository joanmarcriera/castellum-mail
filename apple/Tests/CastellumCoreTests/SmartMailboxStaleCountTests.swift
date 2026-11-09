// Castellum - smart mailbox badges follow local changes; date conditions cover whole days.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Bug hunt 2026-10-12, smart-search:
// - flag, tag and mute changed what a smart mailbox matches but never
//   refreshed its cached counts, so its badge stayed stale until a sync;
// - "is before / is after" stored the clock time the condition was created
//   at (a date-only picker keeps it), so the boundary day was cut mid-day.
// Temp profile and in-memory Keychain; nothing reaches a server.

import XCTest
@testable import Castellum

@MainActor
final class SmartMailboxStaleCountTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-smart-counts-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        let group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .isStarred, op: .is, value: .bool(true)),
        ])
        let req = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "Flagged", query: "", symbol: nil, group: group, scope: nil)
        model.smartMailboxes = [try model.core.upsertSmartMailbox(
            json: try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(req)))]
        model.messages = [header("a:1")]
        model.selectedMessage = model.messages[0]
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func header(_ id: String) -> MessageHeader {
        MessageHeader(id: id, accountId: "a", mailboxId: "a:INBOX",
                      subject: nil, fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: false, isStarred: false, threadCount: 1)
    }

    /// Waits (bounded) for the mutation's main-actor completion to ask for
    /// a smart mailbox count refresh.
    private func waitForRefresh(_ model: AppModel, after before: Int) async {
        for _ in 0..<200 where model.smartMailboxCountRefreshes <= before {
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
    }

    func testFlaggingRefreshesSmartMailboxCounts() async throws {
        let model = try makeModel()
        let before = model.smartMailboxCountRefreshes
        model.setStarred(ids: ["a:1"], starred: true)
        await waitForRefresh(model, after: before)
        XCTAssertGreaterThan(model.smartMailboxCountRefreshes, before)
    }

    func testTaggingRefreshesSmartMailboxCounts() async throws {
        let model = try makeModel()
        let before = model.smartMailboxCountRefreshes
        model.toggleTag("$label1")
        await waitForRefresh(model, after: before)
        XCTAssertGreaterThan(model.smartMailboxCountRefreshes, before)
    }

    func testNoSmartMailboxesMeansNoRefresh() async throws {
        let model = try makeModel()
        model.smartMailboxes = []
        let before = model.smartMailboxCountRefreshes
        model.setStarred(ids: ["a:1"], starred: true)
        try await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(model.smartMailboxCountRefreshes, before)
    }

    // MARK: - whole-day dates

    private var london: Calendar {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "Europe/London")!
        return c
    }

    /// 2026-10-12 15:30 BST, as an epoch.
    private let afternoon: Int64 = 1_791_815_400

    func testTheStoredDayIsItsLocalStartWhateverTimeItWasPicked() {
        let day = FilterConditionDTO.startOfDay(afternoon, calendar: london)
        XCTAssertEqual(day, 1_791_759_600, "2026-10-12 00:00 BST")
    }

    func testEachOperatorCoversWholeDays() {
        let start: Int64 = 1_791_759_600          // 12 Oct 00:00
        let lastSecond: Int64 = start + 86_400 - 1 // 12 Oct 23:59:59
        XCTAssertEqual(FilterConditionDTO.wireDate(op: .before, day: afternoon, calendar: london), start)
        XCTAssertEqual(FilterConditionDTO.wireDate(op: .onOrAfter, day: afternoon, calendar: london), start)
        XCTAssertEqual(FilterConditionDTO.wireDate(op: .after, day: afternoon, calendar: london), lastSecond)
        XCTAssertEqual(FilterConditionDTO.wireDate(op: .onOrBefore, day: afternoon, calendar: london), lastSecond)
    }

    func testTheDayTheClocksGoBackHas25Hours() {
        // 2026-10-25: BST ends, the day is 25 hours long.
        var parts = DateComponents()
        parts.year = 2026; parts.month = 10; parts.day = 25; parts.hour = 12
        let midday = Int64(london.date(from: parts)!.timeIntervalSince1970)
        let start = FilterConditionDTO.startOfDay(midday, calendar: london)
        XCTAssertEqual(FilterConditionDTO.wireDate(op: .after, day: midday, calendar: london) - start,
                       25 * 3600 - 1)
    }

    /// A saved condition from before the fix holds 15:30; it loads as the
    /// day, and goes to the core with the operator's whole-day bound.
    func testASavedMidDayValueLoadsAsItsDayAndEncodesTheWholeDayBound() throws {
        let saved = #"{"field":"date","operator":"after","value":\#(afternoon)}"#
        let c = try JSONDecoder().decode(FilterConditionDTO.self, from: Data(saved.utf8))
        let localStart = FilterConditionDTO.startOfDay(afternoon)
        XCTAssertEqual(c.value, .number(localStart))
        let json = try XCTUnwrap(String(data: JSONEncoder().encode(c), encoding: .utf8))
        XCTAssertTrue(json.contains("\(FilterConditionDTO.wireDate(op: .after, day: localStart))"), json)
        // Round trip is stable: the encoded bound decodes to the same day.
        let again = try JSONDecoder().decode(FilterConditionDTO.self, from: Data(json.utf8))
        XCTAssertEqual(again, c)
    }

    func testANewDateConditionStartsAtTheStartOfToday() {
        let blank = FilterConditionDTO.defaultValue(field: .date, op: .before)
        XCTAssertEqual(blank, .number(FilterConditionDTO.startOfDay(Int64(Date().timeIntervalSince1970))))
    }

    func testRelativeDayConditionsAreUntouched() throws {
        let saved = #"{"field":"date","operator":"within_last_days","value":7}"#
        let c = try JSONDecoder().decode(FilterConditionDTO.self, from: Data(saved.utf8))
        XCTAssertEqual(c.value, .number(7))
        XCTAssertTrue(try XCTUnwrap(String(data: JSONEncoder().encode(c), encoding: .utf8)).contains(":7"))
    }
}
