// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// The upsert payload must be built with JSONEncoder (never string
/// interpolation) so user-typed names with quotes/backslashes stay valid JSON,
/// and `archived == nil` must be OMITTED (the core treats absent as
/// "unchanged").
final class ProjectUpsertEncodingTests: XCTestCase {

    private func decode(_ json: String) throws -> [String: Any] {
        let obj = try JSONSerialization.jsonObject(with: Data(json.utf8))
        return try XCTUnwrap(obj as? [String: Any])
    }

    func testNameWithQuotesProducesValidJSON() throws {
        let json = try XCTUnwrap(AppModel.encodeUpsert(
            AppModel.ProjectUpsertRequest(name: #"Say "hello" \ world"#)))
        let dict = try decode(json)
        XCTAssertEqual(dict["name"] as? String, #"Say "hello" \ world"#)
        XCTAssertEqual(dict["status"] as? String, "active")
    }

    func testArchivedNilIsOmitted() throws {
        let json = try XCTUnwrap(AppModel.encodeUpsert(
            AppModel.ProjectUpsertRequest(id: "p1", name: "X", archived: nil)))
        let dict = try decode(json)
        XCTAssertNil(dict["archived"], "nil archived must be absent, not null")
        XCTAssertEqual(dict["id"] as? String, "p1")
    }

    func testArchivedTrueAndFalseAreEncoded() throws {
        for flag in [true, false] {
            let json = try XCTUnwrap(AppModel.encodeUpsert(
                AppModel.ProjectUpsertRequest(id: "p1", name: "X", archived: flag)))
            let dict = try decode(json)
            XCTAssertEqual(dict["archived"] as? Bool, flag)
        }
    }

    func testNewProjectOmitsId() throws {
        let json = try XCTUnwrap(AppModel.encodeUpsert(
            AppModel.ProjectUpsertRequest(name: "Fresh")))
        let dict = try decode(json)
        XCTAssertNil(dict["id"], "create path must omit id so the core generates one")
    }
}
