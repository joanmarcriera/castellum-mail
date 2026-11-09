// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// The upsert payload must be built with JSONEncoder (never string
/// interpolation) so a user-typed name with quotes/backslashes stays valid
/// JSON, and a create must OMIT `id` (the core generates one).
final class SmartMailboxUpsertEncodingTests: XCTestCase {

    private func decode(_ json: String) throws -> [String: Any] {
        let obj = try JSONSerialization.jsonObject(with: Data(json.utf8))
        return try XCTUnwrap(obj as? [String: Any])
    }

    func testNameWithQuotesProducesValidJSON() throws {
        let json = try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(
            AppModel.SmartMailboxUpsertRequest(name: #"Say "hello" \ world"#, query: "from:x")))
        let dict = try decode(json)
        XCTAssertEqual(dict["name"] as? String, #"Say "hello" \ world"#)
        XCTAssertEqual(dict["query"] as? String, "from:x")
    }

    func testNewSmartMailboxOmitsId() throws {
        let json = try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(
            AppModel.SmartMailboxUpsertRequest(name: "Fresh", query: "is:unread")))
        let dict = try decode(json)
        XCTAssertNil(dict["id"], "create path must omit id so the core generates one")
    }

    func testExistingIdIsIncludedForRename() throws {
        let json = try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(
            AppModel.SmartMailboxUpsertRequest(id: "sm1", name: "Renamed", query: "from:x")))
        let dict = try decode(json)
        XCTAssertEqual(dict["id"] as? String, "sm1")
    }

    func testSymbolNilIsOmitted() throws {
        let json = try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(
            AppModel.SmartMailboxUpsertRequest(name: "X", query: "y")))
        let dict = try decode(json)
        XCTAssertNil(dict["symbol"])
    }

    /// MM-1c: a Header row travels as the object-shaped field the core expects.
    func testHeaderConditionEncodesAsObjectField() throws {
        let group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .header, op: .contains, value: .text("dev.example.org"), headerName: "List-Id"),
        ])
        let json = try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(
            AppModel.SmartMailboxUpsertRequest(name: "Lists", query: "", group: group)))
        let dict = try decode(json)
        let g = try XCTUnwrap(dict["group"] as? [String: Any])
        let cond = try XCTUnwrap((g["conditions"] as? [[String: Any]])?.first)
        XCTAssertEqual((cond["field"] as? [String: String]), ["header": "List-Id"])
        XCTAssertEqual(cond["operator"] as? String, "contains")
    }
}
