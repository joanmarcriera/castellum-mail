// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S41 smart mailboxes: full-stack (Swift -> FFI -> Rust) smoke
// tests — CRUD, hostile queries, and the "unknown id" degrade-not-crash path.
import XCTest
@testable import Castellum

final class SmartMailboxFullStackTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-swift-smart-mailboxes-\(UUID().uuidString)")
    }

    func testCreateListRenameDeleteRoundTrip() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let created = try core.upsertSmartMailbox(json: #"{"name":"Alice","query":"from:alice"}"#)
        XCTAssertEqual(created.name, "Alice")
        XCTAssertEqual(created.cachedTotal, 0, "no mail seeded — recompute finds nothing, not an error")

        let list = try core.listSmartMailboxes()
        XCTAssertEqual(list.map(\.id), [created.id])

        let renamed = try core.upsertSmartMailbox(json: #"{"id":"\#(created.id)","name":"Alice 2","query":"from:alice2"}"#)
        XCTAssertEqual(renamed.id, created.id)
        XCTAssertEqual(renamed.name, "Alice 2")

        try core.deleteSmartMailbox(id: created.id)
        XCTAssertEqual(try core.listSmartMailboxes(), [])
    }

    /// MM-1: a nested none-group + scope, built with the editor's own DTOs,
    /// saved through the real FFI, and read back identical (restart-safe: a
    /// second CastellumCore on the same profile sees the same group).
    func testGroupMailboxBuiltFromEditorDTOsRoundTripsThroughTheCore() throws {
        let dir = tempProfile()
        let core = try CastellumCore(profilePath: dir)
        let group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .isRead, op: .is, value: .bool(false)),
        ], groups: [FilterGroupDTO(op: .none, conditions: [
            FilterConditionDTO(field: .from, op: .contains, value: .text("newsletter")),
            FilterConditionDTO(field: .date, op: .olderThanDays, value: .number(30)),
        ])])
        let req = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "Unread, recent, human", query: "", symbol: nil,
            group: group, scope: SmartMailboxScopeDTO(accountIds: ["nobody"], mailboxIds: []))
        let json = try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(req))
        let saved = try core.upsertSmartMailbox(json: json)
        XCTAssertEqual(saved.query, "")
        XCTAssertEqual(saved.group, group)
        XCTAssertEqual(saved.scope?.accountIds, ["nobody"])

        let reopened = try CastellumCore(profilePath: dir)
        let listed = try XCTUnwrap(try reopened.listSmartMailboxes().first)
        XCTAssertEqual(listed.group, group)
        XCTAssertEqual(try reopened.messagesForSmartMailbox(id: listed.id, limit: 10)?.count, 0)

        // An empty group is refused by the core (the UI also blocks it).
        let empty = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "Nothing", query: "", symbol: nil, group: FilterGroupDTO(op: .and), scope: nil)
        XCTAssertThrowsError(try core.upsertSmartMailbox(json: try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(empty))))
    }

    func testUnknownIdRunDegradesToNilNeverThrows() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        XCTAssertNil(try core.messagesForSmartMailbox(id: "not-an-id", limit: 10))
    }

    /// Every S39 operator plus several hostile strings, saved as a smart
    /// mailbox and then run, all the way through String -> CString -> Rust
    /// parser -> SQL -> back. None of these seed any mail, so every run must
    /// return an empty (non-nil) array without throwing — matching
    /// `SearchQuerySyntaxTests.testHostileSearchQueriesNeverThrow`'s
    /// coverage but through the saved-search path instead of live search.
    func testHostileSavedQueriesNeverThrowOnSaveOrRun() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let queries = [
            "from:alice to:bob cc:carol subject:\"weekly report\"",
            "has:attachment before:2026-09-18 after:2026-01-01",
            "in:inbox is:unread is:flagged -is:read",
            "-spam \"quoted phrase\" bare -\"another phrase\"",
            "-", "-  -", "::::", "\"unterminated quote",
            "café münchën 日本語 emoji 🎉",
        ]
        for (i, q) in queries.enumerated() {
            let sm = try core.upsertSmartMailbox(json: #"{"name":"Q\#(i)","query":\#(String(reflecting: q))}"#)
            let rows = try core.messagesForSmartMailbox(id: sm.id, limit: 10)
            XCTAssertEqual(rows?.count, 0, "query: \(q)")
        }
    }

    /// MM-1c through the real FFI: a Header group saves and reads back
    /// identical, the policy and coverage calls answer, and a built-in or
    /// never-stored name is refused by the core (nothing stored).
    func testHeaderConditionRoundTripPolicyAndRejections() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let policy = try core.filterHeaderPolicy()
        XCTAssertEqual(policy.status(of: "From"), .builtIn(.from))
        XCTAssertEqual(policy.status(of: "Received"), .notStored)
        XCTAssertEqual(policy.status(of: "Authorization"), .notStored)
        XCTAssertEqual(policy.status(of: "List-Id"), .storable)
        XCTAssertEqual(try core.messageHeaderCoverage(), HeaderCoverageDTO(captured: 0, total: 0))

        func request(_ name: String) throws -> String {
            let group = FilterGroupDTO(op: .and, conditions: [
                FilterConditionDTO(field: .header, op: .contains, value: .text("dev"), headerName: name),
            ])
            return try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(
                AppModel.SmartMailboxUpsertRequest(name: "H \(name)", query: "", group: group)))
        }
        let saved = try core.upsertSmartMailbox(json: try request("List-Id"))
        XCTAssertEqual(saved.group?.conditions.first?.headerName, "List-Id")
        XCTAssertEqual(saved.cachedTotal, 0)
        for bad in ["From", "Received", "bad name"] {
            XCTAssertThrowsError(try core.upsertSmartMailbox(json: try request(bad)), bad)
        }
        XCTAssertEqual(try core.listSmartMailboxes().count, 1, "rejected names stored nothing")
    }
}
