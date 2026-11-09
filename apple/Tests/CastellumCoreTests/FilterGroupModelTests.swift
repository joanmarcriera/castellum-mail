// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// MM-1: the Swift side of nested smart-mailbox conditions.
final class FilterGroupModelTests: XCTestCase {
    /// Same strategy `CastellumCore.decodeJSON` uses for every FFI payload.
    private static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    private func decode(_ json: String) throws -> FilterGroupDTO {
        try Self.decoder.decode(FilterGroupDTO.self, from: Data(json.utf8))
    }

    func testDecodesRustShapedNestedNoneGroup() throws {
        let g = try decode(#"""
        {"op":"and","conditions":[{"field":"is_read","operator":"is","value":false}],
         "groups":[{"op":"none","conditions":[
            {"field":"from","operator":"contains","value":"alice"},
            {"field":"date","operator":"within_last_days","value":7},
            {"field":"account_id","operator":"in","value":["a1","a2"]}]}]}
        """#)
        XCTAssertEqual(g.op, .and)
        XCTAssertEqual(g.conditions[0].value, .bool(false))
        XCTAssertEqual(g.groups[0].op, .none)
        XCTAssertEqual(g.groups[0].conditions[1].op, .withinLastDays)
        XCTAssertEqual(g.groups[0].conditions[1].value, .number(7))
        XCTAssertEqual(g.groups[0].conditions[2].field, .accountId)
        XCTAssertEqual(g.groups[0].conditions[2].value, .list(["a1", "a2"]))
        XCTAssertEqual(g.depth, 2)
        XCTAssertEqual(g.conditionCount, 4)
    }

    func testEncodesTheKeysTheCoreExpects() throws {
        let g = FilterGroupDTO(op: .or, conditions: [
            FilterConditionDTO(field: .sizeBytes, op: .gt, value: .number(5)),
            FilterConditionDTO(field: .hasAttachments, op: .is, value: .bool(true)),
        ], groups: [FilterGroupDTO(op: .none, conditions: [
            FilterConditionDTO(field: .attachmentName, op: .startsWith, value: .text("inv")),
        ])])
        let data = try CastellumCore.encoder.encode(g)
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(obj["op"] as? String, "or")
        let first = (obj["conditions"] as! [[String: Any]])[0]
        XCTAssertEqual(first["field"] as? String, "size_bytes")
        XCTAssertEqual(first["operator"] as? String, "gt")
        XCTAssertEqual(first["value"] as? Int, 5)
        XCTAssertNil(first["id"], "the local id is never sent to the core")
        let nested = (obj["groups"] as! [[String: Any]])[0]
        XCTAssertEqual(nested["op"] as? String, "none")
        XCTAssertEqual(((nested["conditions"] as! [[String: Any]])[0])["field"] as? String, "attachment_name")
    }

    func testRoundTripIgnoresLocalIdentity() throws {
        let g = FilterGroupDTO.starter()
        let back = try Self.decoder.decode(
            FilterGroupDTO.self, from: try CastellumCore.encoder.encode(g))
        XCTAssertEqual(back, g)
        XCTAssertNotEqual(back.id, g.id)
    }

    func testEditorOnlyOffersSensibleOperatorsPerField() {
        XCTAssertEqual(FilterFieldKind.body.operators, [.contains])
        XCTAssertEqual(FilterFieldKind.isRead.operators, [.is])
        XCTAssertTrue(FilterFieldKind.date.operators.contains(.withinLastDays))
        XCTAssertFalse(FilterFieldKind.subject.operators.contains(.before))
        for field in FilterFieldKind.editable {
            XCTAssertFalse(field.operators.isEmpty, "\(field)")
            let blank = FilterConditionDTO.blank(field: field)
            XCTAssertTrue(field.operators.contains(blank.op))
        }
    }

    func testTagConditionRoundTripsAndIsEditable() throws {
        XCTAssertTrue(FilterFieldKind.editable.contains(.tag))
        XCTAssertEqual(FilterFieldKind.tag.rawValue, "tag", "wire name matches the Rust enum")
        XCTAssertEqual(FilterFieldKind.tag.operators, [.equals, .is])
        var c = FilterConditionDTO.blank(field: .tag)
        XCTAssertEqual(c.op, .equals)
        XCTAssertEqual(c.value, .text(""))
        XCTAssertFalse(c.isComplete)
        c.value = .text("Work")
        c.changeOp(to: .is)          // has any tag: value re-shapes to a bool
        XCTAssertEqual(c.value, .bool(true))
        XCTAssertEqual(FilterFieldKind.tag.label(for: .is), String(localized: "has"))
        let g = FilterGroupDTO(op: .none, conditions: [
            FilterConditionDTO(field: .tag, op: .equals, value: .text("Work")), c])
        let back = try Self.decoder.decode(
            FilterGroupDTO.self, from: try CastellumCore.encoder.encode(g))
        XCTAssertEqual(back, g)
        let obj = try JSONSerialization.jsonObject(
            with: try CastellumCore.encoder.encode(g)) as! [String: Any]
        XCTAssertEqual(((obj["conditions"] as! [[String: Any]])[0])["field"] as? String, "tag")
    }

    func testTagLabelsExistInEnglishSpanishAndCatalan() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/Castellum/Resources/Localizable.xcstrings")
        let data = try Data(contentsOf: url)
        let root = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        let strings = root["strings"] as! [String: [String: Any]]
        for key in ["Tag", "has", "any tag", "no tag"] {
            let locs = strings[key]?["localizations"] as? [String: Any]
            XCTAssertEqual(Set(locs?.keys.map { $0 } ?? []), ["en", "es", "ca"], key)
        }
    }

    func testChangingFieldResetsOperatorAndValueShape() {
        var c = FilterConditionDTO(field: .subject, op: .contains, value: .text("x"))
        c.changeField(to: .isRead)
        XCTAssertEqual(c.op, .is)
        XCTAssertEqual(c.value, .bool(true))
        c.changeField(to: .date)
        XCTAssertEqual(c.op, .withinLastDays)
        XCTAssertEqual(c.value, .number(7))
        c.changeOp(to: .before)   // days -> absolute date: value re-shaped
        if case .number(let n) = c.value { XCTAssertGreaterThan(n, 1_000_000_000) } else { XCTFail() }
        c.changeOp(to: .after)    // same kind: value kept
    }

    func testSaveValidityAndLimits() {
        XCTAssertFalse(FilterGroupDTO(op: .and).isValidForSave, "empty group")
        XCTAssertFalse(FilterGroupDTO.starter().isValidForSave, "blank subject value")
        var g = FilterGroupDTO.starter()
        g.conditions[0].value = .text("invoice")
        XCTAssertTrue(g.isValidForSave)
        g.groups.append(FilterGroupDTO(op: .none, conditions: [.blank(field: .from)]))
        XCTAssertFalse(g.isValidForSave, "an incomplete nested row blocks saving")
        g.groups[0].conditions[0].value = .text("spam")
        XCTAssertTrue(g.isValidForSave)

        var deep = FilterGroupDTO(op: .and, conditions: [FilterConditionDTO(field: .subject, op: .contains, value: .text("x"))])
        for _ in 0..<FilterGroupDTO.maxDepth { deep = FilterGroupDTO(op: .or, groups: [deep]) }
        XCTAssertFalse(deep.isValidForSave, "deeper than the core allows")
    }

    func testRelativeDaysBoundAndGroupNodeCap() {
        var g = FilterGroupDTO.starter()
        g.conditions = [FilterConditionDTO(field: .date, op: .withinLastDays, value: .number(36_500))]
        XCTAssertTrue(g.isValidForSave)
        g.conditions[0].value = .number(36_501)
        XCTAssertFalse(g.isValidForSave, "core rejects more than 36500 days")
        g.conditions[0].value = .number(-1)
        XCTAssertFalse(g.isValidForSave)
        // non-empty nested groups count as nodes, like the core
        var wide = FilterGroupDTO(op: .and, conditions: [FilterConditionDTO(field: .subject, op: .contains, value: .text("x"))])
        for _ in 0..<FilterGroupDTO.maxConditions {
            wide.groups.append(FilterGroupDTO(op: .or, conditions: [FilterConditionDTO(field: .subject, op: .contains, value: .text("y"))]))
        }
        XCTAssertFalse(wide.isValidForSave, "conditions + groups exceed the cap")
    }

    func testEmptyNestedGroupsArePrunedBeforeSending() {
        var g = FilterGroupDTO.starter()
        g.conditions[0].value = .text("x")
        g.groups = [FilterGroupDTO(op: .or), FilterGroupDTO(op: .or, groups: [FilterGroupDTO(op: .none)])]
        XCTAssertTrue(g.isValidForSave)
        XCTAssertTrue(g.pruningEmptyGroups().groups.isEmpty)
    }

    func testSmartMailboxDecodesWithAndWithoutGroup() throws {
        let legacy = #"{"id":"1","name":"A","query":"from:a","sort_order":0,"symbol":null,"cached_total":0,"cached_unread":0,"counts_capped":false,"counts_updated_at":0,"created_at":1,"updated_at":1}"#
        let sm = try Self.decoder.decode(SmartMailbox.self, from: Data(legacy.utf8))
        XCTAssertNil(sm.group)
        XCTAssertNil(sm.scope)
        let grouped = #"{"id":"2","name":"B","query":"","group":{"op":"none","conditions":[{"field":"body","operator":"contains","value":"x"}],"groups":[]},"scope":{"account_ids":["a"],"mailbox_ids":[]},"sort_order":0,"symbol":null,"cached_total":2,"cached_unread":1,"counts_capped":false,"counts_updated_at":0,"created_at":1,"updated_at":1}"#
        let g = try Self.decoder.decode(SmartMailbox.self, from: Data(grouped.utf8))
        XCTAssertEqual(g.group?.op, FilterBoolOp.none)
        XCTAssertEqual(g.scope?.accountIds, ["a"])
        XCTAssertEqual(g.scope?.mailboxIds, [])
    }

    // MARK: - MM-1c header conditions

    /// A policy as `castellum_filter_header_policy` writes it (fixture subset).
    private static let policyJSON = #"""
    {"max_name_len":64,
     "builtin_fields":{"from":"from","to":"to","cc":"cc","bcc":"bcc","subject":"subject","date":"date"},
     "not_stored":["received","message-id","authorization"],
     "not_stored_prefixes":["x-ms-exchange-"],
     "not_stored_substrings":["token","secret"],
     "known_names":["list-id","list-unsubscribe","x-mailer"]}
    """#

    private func policy() throws -> FilterHeaderPolicy {
        try Self.decoder.decode(FilterHeaderPolicy.self, from: Data(Self.policyJSON.utf8))
    }

    func testDecodesBothFieldShapes() throws {
        let g = try decode(#"""
        {"op":"and","conditions":[
          {"field":"subject","operator":"contains","value":"x"},
          {"field":{"header":"List-Id"},"operator":"contains","value":"dev.example.org"},
          {"field":{"header":"X-Mailer"},"operator":"is","value":false}],"groups":[]}
        """#)
        XCTAssertEqual(g.conditions[0].field, .subject)
        XCTAssertNil(g.conditions[0].headerName)
        XCTAssertEqual(g.conditions[1].field, .header)
        XCTAssertEqual(g.conditions[1].headerName, "List-Id")
        XCTAssertEqual(g.conditions[2].op, .is)
        XCTAssertEqual(g.conditions[2].value, .bool(false))
    }

    func testEncodesHeaderAsObjectAndUnitFieldsAsStrings() throws {
        let g = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .header, op: .contains, value: .text("dev"), headerName: "List-Id"),
            FilterConditionDTO(field: .subject, op: .contains, value: .text("x")),
        ])
        let data = try CastellumCore.encoder.encode(g)
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let conds = try XCTUnwrap(obj["conditions"] as? [[String: Any]])
        XCTAssertEqual((conds[0]["field"] as? [String: String])?["header"], "List-Id")
        XCTAssertEqual(conds[1]["field"] as? String, "subject")
        // Round trip keeps the typed (not lower-cased) name.
        XCTAssertEqual(try Self.decoder.decode(FilterGroupDTO.self, from: data), g)
    }

    func testBlankHeaderRowAndChangeField() {
        let blank = FilterConditionDTO.blank(field: .header)
        XCTAssertEqual(blank.headerName, "")
        XCTAssertEqual(blank.op, .contains)
        XCTAssertFalse(blank.isComplete, "no name, no value")
        XCTAssertEqual(FilterFieldKind.header.operators, [.contains, .equals, .startsWith, .is])
        XCTAssertEqual(FilterFieldKind.header.valueKind(for: .is), .bool)
        XCTAssertEqual(FilterFieldKind.header.valueKind(for: .contains), .text)
        XCTAssertEqual(FilterFieldKind.header.boolLabel(true), "present")
        var row = blank
        row.headerName = "List-Id"
        row.changeField(to: .subject)
        XCTAssertNil(row.headerName, "changeField clears the header name")
        row.changeField(to: .header)
        XCTAssertEqual(row.headerName, "")
    }

    func testIsCompleteAgainstPolicy() throws {
        let p = try policy()
        func row(_ name: String, _ text: String = "x") -> FilterConditionDTO {
            FilterConditionDTO(field: .header, op: .contains, value: .text(text), headerName: name)
        }
        XCTAssertTrue(row("List-Id").isComplete(policy: p))
        XCTAssertTrue(row(" list-id: ").isComplete(policy: p), "policy normalises like the core")
        XCTAssertFalse(row("From").isComplete(policy: p), "built-in field")
        XCTAssertFalse(row("Received").isComplete(policy: p), "never stored")
        XCTAssertFalse(row("X-MS-Exchange-Foo").isComplete(policy: p), "denylisted prefix")
        XCTAssertFalse(row("X-Api-Token").isComplete(policy: p), "credential-like substring")
        XCTAssertFalse(row("bad name").isComplete(policy: p), "invalid")
        XCTAssertFalse(row(String(repeating: "a", count: 65)).isComplete(policy: p))
        XCTAssertFalse(row("").isComplete(policy: p))
        XCTAssertFalse(row("List-Id", "  ").isComplete(policy: p), "blank value")
        // Without a policy only the blank checks apply (the core validates on save).
        XCTAssertTrue(row("From").isComplete(policy: nil))
        let present = FilterConditionDTO(field: .header, op: .is, value: .bool(true), headerName: "List-Id")
        XCTAssertTrue(present.isComplete(policy: p))
        XCTAssertTrue(FilterGroupDTO(conditions: [present]).isValidForSave(policy: p))
        XCTAssertFalse(FilterGroupDTO(conditions: [row("Received")]).isValidForSave(policy: p))
        XCTAssertTrue(FilterGroupDTO(conditions: [row("Received")]).containsHeaderCondition)
    }

    func testPolicyStatusAndCompletions() throws {
        let p = try policy()
        XCTAssertEqual(p.status(of: "From"), .builtIn(.from))
        XCTAssertEqual(p.status(of: "Subject"), .builtIn(.subject))
        XCTAssertEqual(p.status(of: "Message-ID"), .notStored)
        XCTAssertEqual(p.status(of: "X-Mailer"), .storable)
        XCTAssertEqual(p.status(of: "Näme"), .invalid)
        XCTAssertEqual(p.completions(for: "li"), ["list-id", "list-unsubscribe"])
        XCTAssertEqual(p.completions(for: "LIST-ID"), [], "exact match needs no completion")
        XCTAssertEqual(p.completions(for: ""), [])
    }
    /// MM-1c slice 1: a group with a Header row (at any depth) never saves as
    /// an Inbox split; other groups keep the user's choice.
    func testHeaderConditionsClearInboxSplitOnSave() {
        let split = SmartMailboxScopeDTO(accountIds: ["a"], inboxSplit: true)
        let header = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .header, op: .is, value: .bool(true), headerName: "List-Id"),
        ])
        let nested = FilterGroupDTO(op: .or, conditions: [
            FilterConditionDTO(field: .subject, op: .contains, value: .text("x")),
        ], groups: [header])
        let plain = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .subject, op: .contains, value: .text("x")),
        ])
        XCTAssertFalse(split.forSave(group: header).inboxSplit)
        XCTAssertFalse(split.forSave(group: nested).inboxSplit)
        XCTAssertEqual(split.forSave(group: header).accountIds, ["a"], "only the split flag changes")
        XCTAssertTrue(split.forSave(group: plain).inboxSplit)
    }
    /// RulesView hides Header, but an existing Header row must still find its
    /// own tag in the field picker (it would otherwise render blank).
    func testPickerOptionsKeepTheRowsOwnField() {
        let rules = FilterFieldKind.editable.filter { $0 != .header }
        XCTAssertEqual(FilterFieldKind.pickerOptions(offered: rules, current: .subject), rules)
        XCTAssertEqual(FilterFieldKind.pickerOptions(offered: rules, current: .header), rules + [.header])
        XCTAssertEqual(
            FilterFieldKind.pickerOptions(offered: FilterFieldKind.editable, current: .header),
            FilterFieldKind.editable)
    }
}
