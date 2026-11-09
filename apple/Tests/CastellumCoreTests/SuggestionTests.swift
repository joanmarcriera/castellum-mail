// Castellum - suggestions you accept: Swift DTO decode + live FFI calls.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class SuggestionTests: XCTestCase {
    private func decoder() -> JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }

    func testSuggestionDecodesAWaitingForReplyProposal() throws {
        let json = """
        {"id": "s1", "kind": "waiting_for_reply", "status": "proposed",
         "payload": {"title": "Quote", "due_at": 1800000000},
         "message_id": "m1", "thread_id": "t1", "extraction_id": null,
         "rfc822_message_id": "<a@b>", "target_work_item_id": null,
         "created_work_item_id": null, "origin": "heuristic",
         "confidence": null, "dedupe_key": "sent:acc:4",
         "created_at": 100, "decided_at": null}
        """
        let s = try decoder().decode(Suggestion.self, from: Data(json.utf8))
        XCTAssertEqual(s.kind, "waiting_for_reply")
        XCTAssertEqual(s.status, "proposed")
        XCTAssertEqual(s.payload.title, "Quote")
        XCTAssertEqual(s.payload.dueAt, 1_800_000_000)
        XCTAssertNil(s.payload.toState)
        XCTAssertEqual(s.messageId, "m1")
        XCTAssertEqual(s.threadId, "t1")
        XCTAssertNil(s.extractionId)
        XCTAssertEqual(s.rfc822MessageId, "<a@b>")
        XCTAssertEqual(s.dedupeKey, "sent:acc:4")
        XCTAssertNil(s.decidedAt)
    }

    func testSuggestionDecodesAStateChangeWithoutOptionalKeys() throws {
        // Optional keys may be missing entirely, not just null.
        let json = """
        {"id": "s2", "kind": "state_change", "status": "accepted",
         "payload": {"to_state": "needs_reply"},
         "target_work_item_id": "w1", "origin": "heuristic",
         "dedupe_key": "reply:w1:5", "created_at": 1, "decided_at": 9}
        """
        let s = try decoder().decode(Suggestion.self, from: Data(json.utf8))
        XCTAssertEqual(s.payload.toState, "needs_reply")
        XCTAssertNil(s.payload.title)
        XCTAssertEqual(s.targetWorkItemId, "w1")
        XCTAssertEqual(s.decidedAt, 9)
        XCTAssertNil(s.confidence)
    }

    func testSuggestionAcceptedDecodesTheCreatedWorkItem() throws {
        let json = """
        {"suggestion": {"id": "s1", "kind": "work_item", "status": "accepted",
          "payload": {"title": "Call"}, "origin": "ai_extraction",
          "confidence": 0.5, "dedupe_key": "k", "created_at": 1,
          "created_work_item_id": "w9", "decided_at": 2},
         "work_item": {"id": "w9", "project_id": null, "title": "Call",
          "notes": "", "state": "needs_reply", "position": 0,
          "created_at": 2, "updated_at": 2}}
        """
        let a = try decoder().decode(SuggestionAccepted.self, from: Data(json.utf8))
        XCTAssertEqual(a.suggestion.createdWorkItemId, "w9")
        XCTAssertEqual(a.suggestion.confidence, 0.5)
        XCTAssertEqual(a.workItem?.id, "w9")
        XCTAssertNil(a.workItem?.projectId)
    }

    /// A fresh temp profile has nothing proposed; the wrappers round-trip the
    /// empty case and surface Rust errors (never touches the real profile).
    func testEmptyProfileListCountAndErrorsOverFFI() throws {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-suggestions-test-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let core = try CastellumCore(profilePath: dir)
        XCTAssertEqual(try core.listSuggestions(), [])
        XCTAssertEqual(try core.listSuggestions(filterJSON: "{\"status\":\"proposed\"}"), [])
        XCTAssertEqual(try core.countSuggestions(), 0)
        XCTAssertThrowsError(try core.listSuggestions(filterJSON: "{\"bogus\":1}"))
        XCTAssertThrowsError(try core.acceptSuggestion(id: "missing"))
        XCTAssertThrowsError(try core.dismissSuggestion(id: "missing"))
    }
}
