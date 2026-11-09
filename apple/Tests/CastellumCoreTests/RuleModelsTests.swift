// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// MM-3: rule DTOs against the Rust JSON shapes, the draft's
/// save rules, and a full Swift -> FFI -> Rust rule round trip.
final class RuleModelsTests: XCTestCase {
    private static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    private func encode<T: Encodable>(_ v: T) throws -> Any {
        try JSONSerialization.jsonObject(with: try CastellumCore.encoder.encode(v), options: [.fragmentsAllowed])
    }

    func testActionsEncodeToTheRustShapes() throws {
        XCTAssertEqual(try encode(RuleActionDTO.markRead) as? String, "mark_read")
        XCTAssertEqual(try encode(RuleActionDTO.markUnread) as? String, "mark_unread")
        XCTAssertEqual(try encode(RuleActionDTO.archive) as? String, "archive")
        XCTAssertEqual(try encode(RuleActionDTO.trash) as? String, "trash")
        let move = try encode(RuleActionDTO.move(mailboxId: "a:Work")) as? [String: [String: String]]
        XCTAssertEqual(move, ["move": ["mailbox_id": "a:Work"]])
        let star = try encode(RuleActionDTO.addStar) as? [String: [String: String]]
        XCTAssertEqual(star, ["add_flag": ["flag": "starred"]])
        let unstar = try encode(RuleActionDTO.removeStar) as? [String: [String: String]]
        XCTAssertEqual(unstar, ["remove_flag": ["flag": "starred"]])
        let tag = try encode(RuleActionDTO.addTag(keyword: "$label1")) as? [String: [String: String]]
        XCTAssertEqual(tag, ["add_tag": ["keyword": "$label1"]])
    }

    func testActionsDecodeFromRustJSON() throws {
        let json = #"["mark_read","mark_unread","archive","trash",{"move":{"mailbox_id":"a:Work"}},{"add_flag":{"flag":"starred"}},{"remove_flag":{"flag":"starred"}},{"add_tag":{"keyword":"$label1"}}]"#
        let actions = try Self.decoder.decode([RuleActionDTO].self, from: Data(json.utf8))
        XCTAssertEqual(actions, [.markRead, .markUnread, .archive, .trash, .move(mailboxId: "a:Work"), .addStar, .removeStar, .addTag(keyword: "$label1")])
        XCTAssertThrowsError(try Self.decoder.decode([RuleActionDTO].self, from: Data(#"["forward"]"#.utf8)))
    }

    func testDraftSaveRules() {
        var d = RuleDraft()
        XCTAssertNotNil(d.validationProblem, "empty name")
        d.name = "Invoices"
        XCTAssertNotNil(d.validationProblem, "blank condition")
        d.group.conditions[0].value = .text("invoice")
        XCTAssertNotNil(d.validationProblem, "no actions")
        d.actions = [.markRead]
        XCTAssertTrue(d.isValid)
        d.actions = [.archive, .trash]
        XCTAssertNotNil(d.validationProblem, "two row-removing actions")
        d.actions = [.move(mailboxId: "a:Work")]
        XCTAssertNotNil(d.validationProblem, "Move needs one account")
        d.accountId = "a"
        XCTAssertTrue(d.isValid)
        XCTAssertTrue(d.canOfferMove)
    }

    /// FP21: a Move folder of account "a" cannot sit in a rule for account "b".
    func testMoveFolderMustBelongToTheRulesAccount() {
        var d = RuleDraft()
        d.name = "Invoices"
        d.group.conditions[0].value = .text("invoice")
        d.accountId = "b"
        d.actions = [.move(mailboxId: "a:Work")]
        XCTAssertNotNil(d.validationProblem)
        XCTAssertFalse(d.isValid)
        d.actions = [.move(mailboxId: "b:Work")]
        XCTAssertTrue(d.isValid)
    }

    /// FP21: a Move folder that is gone is detected and never shown as a raw id.
    func testRuleWithMissingMoveFolderIsReportedAsBroken() {
        var d = RuleDraft()
        d.accountId = "a1"
        d.actions = [.move(mailboxId: "a1:Gone")]
        XCTAssertEqual(d.missingMoveFolder(mailboxIds: ["a1:INBOX"]), "a1:Gone")
        XCTAssertNil(d.missingMoveFolder(mailboxIds: ["a1:INBOX", "a1:Gone"]))
        let label = RuleActionDTO.move(mailboxId: "a1:Gone").label(mailboxName: { _ in nil })
        XCTAssertFalse(label.contains("a1:Gone"), label)
    }

    func testRunOrderShowsRowRemovingActionsLast() {
        var d = RuleDraft()
        d.actions = [.archive, .markRead, .addStar]
        XCTAssertEqual(d.actionsInRunOrder, [.markRead, .addStar, .archive])
    }

    func testStoredRuleDecodesAndRoundTripsIntoADraft() throws {
        let json = #"""
        {"id":"r1","account_id":"a1","name":"Invoices","kind":"rule",
         "group":{"op":"and","conditions":[{"field":"subject","operator":"contains","value":"invoice"}],"groups":[]},
         "sort_field":null,"sort_desc":true,"trigger":"on_sync","actions":["mark_read"],
         "sort_order":2,"stop_processing":true,"enabled":false,"created_at":"","updated_at":""}
        """#
        let rule = try Self.decoder.decode(SavedRule.self, from: Data(json.utf8))
        XCTAssertEqual(rule.sortOrder, 2)
        XCTAssertTrue(rule.stopProcessing)
        XCTAssertFalse(rule.enabled)
        let draft = RuleDraft(rule)
        XCTAssertEqual(draft.id, "r1")
        XCTAssertEqual(draft.accountId, "a1")
        XCTAssertEqual(draft.trigger, .onSync)
        XCTAssertFalse(draft.enabled)
    }

    func testRuleRoundTripThroughTheRealCore() throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-swift-rules-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        var draft = RuleDraft()
        draft.name = "Newsletters"
        draft.group = FilterGroupDTO(op: .or, conditions: [
            FilterConditionDTO(field: .from, op: .contains, value: .text("newsletter")),
        ])
        draft.actions = [.markRead, .archive]
        draft.stopProcessing = true
        let saved = try core.upsertFilter(json: String(decoding: try CastellumCore.encoder.encode(RuleUpsertRequest(draft)), as: UTF8.self))
        XCTAssertEqual(saved.name, "Newsletters")
        XCTAssertEqual(saved.actions, [.markRead, .archive])
        XCTAssertTrue(saved.stopProcessing)
        XCTAssertTrue(saved.enabled)

        var second = RuleDraft()
        second.name = "Second"
        second.group = draft.group
        second.actions = [.markUnread]
        let saved2 = try core.upsertFilter(json: String(decoding: try CastellumCore.encoder.encode(RuleUpsertRequest(second)), as: UTF8.self))
        XCTAssertGreaterThan(saved2.sortOrder, saved.sortOrder, "new rules append")

        try core.reorderFilters(orderedIdsJSON: #"["\#(saved2.id)","\#(saved.id)"]"#)
        XCTAssertEqual(try core.listFilters(accountId: nil).map(\.id), [saved2.id, saved.id])

        // Dry runs change nothing and need no mail.
        let groupJSON = String(decoding: try CastellumCore.encoder.encode(draft.group), as: UTF8.self)
        XCTAssertEqual(try core.evaluateFilter(groupJSON: groupJSON, accountId: nil, limit: 10).count, 0)
        XCTAssertEqual(try core.previewRule(id: saved.id, accountId: nil, limit: 10).matches.count, 0)
        XCTAssertEqual(try core.previewRule(id: saved.id, accountId: nil, limit: 10).total, 0)
        XCTAssertEqual(try core.previewRuleConditions(groupJSON: groupJSON, accountId: nil, limit: 10).total, 0)
        // Run Now in batches: nothing to act on, so one empty batch ends it.
        let batch = try core.runRuleNowBatch(id: saved.id, accountId: nil, after: nil, limit: RuleRunNow.batchSize)
        XCTAssertEqual(batch.matchedCount, 0)
        XCTAssertNil(batch.nextAfter)

        // The core refuses a second action that moves the message (the
        // editor does too), rather than silently dropping it.
        var twoMoves = RuleDraft()
        twoMoves.name = "Two moves"
        twoMoves.group = draft.group
        twoMoves.actions = [.archive, .trash]
        XCTAssertThrowsError(try core.upsertFilter(
            json: String(decoding: try CastellumCore.encoder.encode(RuleUpsertRequest(twoMoves)), as: UTF8.self)))

        // A refused disabled toggle round trip: enabled=false persists.
        var off = RuleDraft(saved)
        off.enabled = false
        let toggled = try core.upsertFilter(json: String(decoding: try CastellumCore.encoder.encode(RuleUpsertRequest(off)), as: UTF8.self))
        XCTAssertFalse(toggled.enabled)
        try core.deleteFilter(id: saved.id)
        XCTAssertEqual(try core.listFilters(accountId: nil).map(\.id), [saved2.id])
    }

    /// MM-3 review: above the per-run cap the confirmation says only the
    /// first N are processed (never a bare, capped count).
    func testRunConfirmationNamesTheCapWhenTheMatchSetIsLarger() {
        let small = RulesSettingsTab.runConfirmation(actions: "archive", count: 3)
        XCTAssertTrue(small.contains("3 current messages"), small)
        let large = RulesSettingsTab.runConfirmation(actions: "archive", count: 78_012)
        XCTAssertTrue(large.contains("the first \(RuleRunNow.maxMessages.formatted())"), large)
        XCTAssertTrue(large.contains(78_012.formatted()), large)
        XCTAssertTrue(large.contains("+"), large)
    }

    /// R1: a capped run points at Continue (not "run it again", which restarts),
    /// and the report line says how many remain.
    func testCappedRunPointsAtContinueAndNamesWhatRemains() {
        let large = RulesSettingsTab.runConfirmation(actions: "archive", count: 78_012)
        XCTAssertTrue(large.contains("Continue"), large)
        XCTAssertFalse(large.contains("run it again"), large)
        XCTAssertTrue(RuleRunNow.remainingText(1_204).contains(1_204.formatted()))
        XCTAssertEqual(RuleRunNow.remainingText(nil), "More messages")
    }

    /// Review fix: toggling must round-trip the stored rule untouched (sort
    /// field/direction, trigger, actions, stop flag), even for a rule the
    /// editor itself would refuse to save.
    func testEnableSwitchKeepsEveryStoredField() throws {
        let json = #"""
        {"id":"r1","account_id":null,"name":"Legacy","kind":"rule",
         "group":{"op":"and","conditions":[{"field":"subject","operator":"contains","value":"x"}],"groups":[]},
         "sort_field":"size","sort_desc":false,"trigger":"manual",
         "actions":[{"move":{"mailbox_id":"a1:Work"}}],
         "sort_order":0,"stop_processing":true,"enabled":true,"created_at":"","updated_at":""}
        """#
        let rule = try Self.decoder.decode(SavedRule.self, from: Data(json.utf8))
        XCTAssertFalse(RuleDraft(rule).isValid, "an all-accounts Move fails the editor's save rules")
        let obj = try JSONSerialization.jsonObject(
            with: try CastellumCore.encoder.encode(RuleUpsertRequest(rule: rule, enabled: false))) as! [String: Any]
        XCTAssertEqual(obj["enabled"] as? Bool, false)
        XCTAssertEqual(obj["sort_field"] as? String, "size")
        XCTAssertEqual(obj["sort_desc"] as? Bool, false)
        XCTAssertEqual(obj["trigger"] as? String, "manual")
        XCTAssertEqual(obj["stop_processing"] as? Bool, true)
        XCTAssertEqual((obj["actions"] as? [[String: Any]])?.count, 1)
        XCTAssertNil(obj["account_id"] as Any? as? String)
    }

    /// #3146: the name is mandatory and flagged on its own, and the Save hint says so.
    func testBlankNameIsFlaggedAsMissing() {
        var d = RuleDraft()
        d.name = "   "
        XCTAssertTrue(d.nameIsMissing)
        XCTAssertEqual(d.validationProblem, "Give the rule a name.")
        d.name = "Newsletters"
        XCTAssertFalse(d.nameIsMissing)
    }

    func testAddTagActionIsAValidActionAndLabelled() {
        var d = RuleDraft()
        d.name = "Tag it"
        d.actions = [.addTag(keyword: "Work")]
        XCTAssertNotEqual(d.validationProblem, "Add at least one action.")
        XCTAssertEqual(RuleActionDTO.addTag(keyword: "Work").label(mailboxName: { _ in nil }), "Add tag Work")
    }

    /// #3146: Remove tag uses the Rust wire shape both ways.
    func testRemoveTagRoundTripsTheRustWireShape() throws {
        let tag = try encode(RuleActionDTO.removeTag(keyword: "Work")) as? [String: [String: String]]
        XCTAssertEqual(tag, ["remove_tag": ["keyword": "Work"]])
        let decoded = try Self.decoder.decode(
            [RuleActionDTO].self, from: Data(#"[{"remove_tag":{"keyword":"Work"}}]"#.utf8))
        XCTAssertEqual(decoded, [.removeTag(keyword: "Work")])
        XCTAssertEqual(RuleActionDTO.removeTag(keyword: "Work").label(mailboxName: { _ in nil }), "Remove tag Work")
        XCTAssertFalse(RuleActionDTO.removeTag(keyword: "Work").removesMessage)
    }

    func testAddingAndRemovingTheSameTagIsExplainedBeforeSave() {
        var d = RuleDraft()
        d.name = "Swap"
        d.group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .subject, op: .contains, value: .text("x")),
        ])
        d.actions = [.addTag(keyword: "Work"), .removeTag(keyword: "work")]
        XCTAssertTrue(d.addsAndRemovesTheSameTag)
        XCTAssertEqual(d.validationProblem, "A rule can’t both add and remove the same tag.")
        d.actions = [.addTag(keyword: "Done"), .removeTag(keyword: "Todo")]
        XCTAssertFalse(d.addsAndRemovesTheSameTag)
        XCTAssertNil(d.validationProblem)
    }
}
