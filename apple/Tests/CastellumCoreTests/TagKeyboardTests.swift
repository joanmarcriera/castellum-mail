// Castellum - MM-8 slice 2: digit keys 1-9 toggle the first nine tags, `g l` opens a
// tag's list through the tag picker. Registry shape, router behaviour (focus
// stand-down, empty slots, modifiers, rebinding), AppModel slot semantics, palette
// availability and the go-to matcher.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class TagKeyboardRegistryTests: XCTestCase {
    func testNineSlotCommandsExistWithBareDigitsAndTheirOwnNotification() {
        for n in 1...9 {
            let id = try! XCTUnwrap(CommandID.tagSlot(n))
            XCTAssertEqual(id.tagSlotIndex, n)
            let spec = CommandRegistry.spec(for: id)
            XCTAssertEqual(spec.bareKey, Character(String(n)))
            XCTAssertEqual(spec.notification, Notification.Name.castellumTagSlot(n))
            XCTAssertEqual(CommandRegistry.command(forBareKey: Character(String(n)))?.id, id)
        }
        XCTAssertNil(CommandID.tagMessage.tagSlotIndex)
        XCTAssertEqual(Set((1...9).map { Notification.Name.castellumTagSlot($0) }).count, 9)
    }

    func testRegistryStaysConflictFreeWithTheDigitsAndGL() {
        XCTAssertEqual(CommandRegistry.conflicts(in: CommandRegistry.all), [])
    }

    func testGoToTagResolvesOnGL() {
        XCTAssertEqual(CommandRegistry.command(forGChord: "l")?.id, .gotoTag)
        XCTAssertEqual(CommandRegistry.spec(for: .gotoTag).notification, .castellumGotoTag)
        XCTAssertNil(CommandID.gotoTag.mailboxRole)
    }

    func testDigitsAreRebindableLikeAnyCommand() {
        let base = CommandRegistry.spec(for: .tagSlot1)
        let patched = base.applyingOverride(KeyBindingOverride(menuChord: nil, bareKey: "w"))
        XCTAssertEqual(patched.bareKey, "w")
        XCTAssertEqual(patched.notification, base.notification)
    }

    func testCheatSheetListsTheSlotsAndTheGoToChord() {
        let groups = CommandRegistry.sheetGroups()
        let act = groups.first { $0.title == SheetGroup.actOnSelection.title }
        XCTAssertTrue(act?.items.contains { $0.keys == ["1"] } ?? false)
        XCTAssertTrue(act?.items.contains { $0.keys == ["9"] } ?? false)
        let goto = groups.first { $0.title == SheetGroup.gotoMailbox.title }
        XCTAssertTrue(goto?.items.contains { $0.keys == ["G L"] } ?? false)
    }

    func testPaletteEnablesASlotOnlyWithASelectionAndATagInIt() {
        var ctx = PaletteContext(hasSelectedMessage: true, messageBodyAvailable: true, aiEnabled: false)
        ctx.tagCount = 2
        XCTAssertTrue(CommandPaletteAvailability.isEnabled(.tagSlot1, context: ctx))
        XCTAssertTrue(CommandPaletteAvailability.isEnabled(.tagSlot2, context: ctx))
        XCTAssertFalse(CommandPaletteAvailability.isEnabled(.tagSlot3, context: ctx), "empty slot")
        ctx.hasSelectedMessage = false
        XCTAssertFalse(CommandPaletteAvailability.isEnabled(.tagSlot1, context: ctx), "no selection")
    }

    func testRetitledKeepsTheBindingAndChangesOnlyTheLabel() {
        let base = CommandRegistry.spec(for: .tagSlot1)
        let named = base.retitled("Toggle Tag 1: Work")
        XCTAssertEqual(named.title, "Toggle Tag 1: Work")
        XCTAssertEqual(named.help, "Toggle Tag 1: Work")
        XCTAssertEqual(named.bareKey, "1")
        XCTAssertEqual(named.notification, base.notification)
    }

    func testGoToMatcherPicksHighlightedElseFirstAndNeverInvents() {
        func tag(_ name: String) -> TagDefinition {
            TagDefinition(keyword: name, keywordKey: name.lowercased(), displayName: name, colour: "blue", sortOrder: 0)
        }
        let tags = [tag("Work"), tag("Later"), tag("Feina")]
        XCTAssertEqual(TagGoToMatcher.tag(highlighted: nil, query: "la", in: tags)?.displayName, "Later")
        XCTAssertEqual(TagGoToMatcher.tag(highlighted: nil, query: "FEI", in: tags)?.displayName, "Feina")
        XCTAssertEqual(TagGoToMatcher.tag(highlighted: "Feina", query: "", in: tags)?.displayName, "Feina")
        XCTAssertEqual(TagGoToMatcher.tag(highlighted: "Work", query: "la", in: tags)?.displayName, "Later",
                       "a stale highlight that no longer matches the filter is ignored")
        XCTAssertNil(TagGoToMatcher.tag(highlighted: nil, query: "zzz", in: tags))
    }
}

@MainActor
final class TagKeyboardRouterTests: XCTestCase {
    private final class Box {
        var posted: [Notification.Name] = []
        var enabled = true
    }

    private func makeRouter(_ box: Box, textEditing: Bool = false) -> KeySequenceRouter {
        KeySequenceRouter(
            isTextEditing: { textEditing },
            isEnabled: { _ in box.enabled },
            post: { box.posted.append($0) })
    }

    func testADigitWithATagInItsSlotIsConsumedAndPostsItsSlotNotification() {
        let box = Box()
        let router = makeRouter(box)
        XCTAssertEqual(router.press("2"), .consumed)
        XCTAssertEqual(box.posted, [.castellumTagSlot(2)])
    }

    func testADigitPassesThroughWhenItsSlotIsEmptyOrNothingIsSelected() {
        let box = Box()
        box.enabled = false
        let router = makeRouter(box)
        XCTAssertEqual(router.press("5"), .passthrough)
        XCTAssertTrue(box.posted.isEmpty)
    }

    func testDigitsPassThroughWhileATextFieldOrComposeHasFocus() {
        let box = Box()
        let router = makeRouter(box, textEditing: true)
        for n in 1...9 {
            XCTAssertEqual(router.press(String(n)), .passthrough)
        }
        XCTAssertTrue(box.posted.isEmpty, "typing 12 in search must not tag anything")
    }

    func testDigitsWithAModifierPassThroughToTheMenus() {
        let box = Box()
        let router = makeRouter(box)
        XCTAssertEqual(router.press("1", hasCommand: true), .passthrough)
        XCTAssertTrue(box.posted.isEmpty)
    }

    func testGThenLPostsTheGoToTagNotification() {
        let box = Box()
        let router = makeRouter(box)
        XCTAssertEqual(router.press("g"), .consumed)
        XCTAssertEqual(router.press("l"), .consumed)
        XCTAssertEqual(box.posted, [.castellumGotoTag])
    }

    func testARebindDigitFollowsTheLiveSpecs() {
        let box = Box()
        let specs = CommandRegistry.all.map {
            $0.id == .tagSlot1 ? $0.applyingOverride(KeyBindingOverride(menuChord: nil, bareKey: "w")) : $0
        }
        let router = KeySequenceRouter(
            specs: specs, isTextEditing: { false }, isEnabled: { _ in true },
            post: { box.posted.append($0) })
        XCTAssertEqual(router.press("w"), .consumed)
        XCTAssertEqual(box.posted, [.castellumTagSlot(1)])
        XCTAssertEqual(router.press("1"), .passthrough, "the freed digit is unbound")
    }
}

@MainActor
final class TagKeyboardModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-tagkeys-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 5,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline { try await Task.sleep(nanoseconds: 20_000_000) }
        if !condition() { XCTFail(message()) }
    }

    private func seedSelectedMessages(_ model: AppModel, count: Int) throws -> [String] {
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId,
            email: "a@example.test", mailboxId: mailboxId)
        var ids: [String] = []
        var headers: [MessageHeader] = []
        for n in 1...count {
            let id = "\(mailboxId):\(n)"
            try DirectFixture.seedMessage(
                profileDirectory: model.profileDirectory, id: id, accountId: accountId,
                mailboxId: mailboxId, subject: "Subject \(n)", fromAddress: "a@example.test",
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
            ids.append(id)
            headers.append(MessageHeader(
                id: id, accountId: accountId, mailboxId: mailboxId, subject: "Subject \(n)", fromName: nil,
                fromAddress: "a@example.test", sentAt: nil, snippet: nil, hasAttachments: false,
                hasBody: nil, isRead: false, isStarred: false, threadCount: 1))
        }
        model.messages = headers
        model.selectedMessageIds = ids
        return ids
    }

    func testSlotsFollowSettingsOrder() throws {
        let model = makeModel()
        let ids = try seedSelectedMessages(model, count: 1)
        _ = model.createTag(displayName: "Work", colour: "blue")
        _ = model.createTag(displayName: "Later", colour: "orange")
        model.toggleTagSlot(2)
        XCTAssertEqual(model.messages.first { $0.id == ids[0] }?.keywords, ["Later"])
        model.toggleTagSlot(1)
        XCTAssertEqual(Set(model.messages[0].keywords), ["Work", "Later"])
    }

    func testTheSameSlotTwiceRemovesTheTagAndEmptySlotsDoNothing() throws {
        let model = makeModel()
        _ = try seedSelectedMessages(model, count: 1)
        _ = model.createTag(displayName: "Work", colour: "blue")
        model.toggleTagSlot(1)
        XCTAssertEqual(model.messages[0].keywords, ["Work"])
        model.toggleTagSlot(1)
        XCTAssertEqual(model.messages[0].keywords, [])
        model.toggleTagSlot(2)
        model.toggleTagSlot(0)
        model.toggleTagSlot(10)
        XCTAssertEqual(model.messages[0].keywords, [])
    }

    func testAddIfAnyLacksTheTagAppliesToTheWholeSelection() throws {
        let model = makeModel()
        _ = try seedSelectedMessages(model, count: 3)
        _ = model.createTag(displayName: "Work", colour: "blue")
        model.messages[0].keywords = ["Work"]
        model.toggleTagSlot(1)
        XCTAssertTrue(model.messages.allSatisfy { $0.keywords == ["Work"] }, "mixed selection: add to all")
        model.toggleTagSlot(1)
        XCTAssertTrue(model.messages.allSatisfy { $0.keywords.isEmpty }, "all have it: remove from all")
    }

    func testBareKeyAvailabilityNeedsASelectionAndATagInTheSlot() throws {
        let model = makeModel()
        XCTAssertFalse(model.isBareKeyCommandAvailable(.tagSlot1), "no tags")
        _ = model.createTag(displayName: "Work", colour: "blue")
        XCTAssertFalse(model.isBareKeyCommandAvailable(.tagSlot1), "no selection")
        _ = try seedSelectedMessages(model, count: 1)
        XCTAssertTrue(model.isBareKeyCommandAvailable(.tagSlot1))
        XCTAssertFalse(model.isBareKeyCommandAvailable(.tagSlot2), "empty slot")
        XCTAssertTrue(model.isBareKeyCommandAvailable(.archive), "other commands are never gated")
    }

    func testGoToSelectsTheTagAndNeverCreatesOne() throws {
        let model = makeModel()
        _ = model.createTag(displayName: "Later", colour: "orange")
        let before = model.tags.count
        let tag = try XCTUnwrap(TagGoToMatcher.tag(highlighted: nil, query: "la", in: model.tags))
        model.select(tag: tag)
        XCTAssertEqual(model.selectedTagKey, "later")
        XCTAssertNil(TagGoToMatcher.tag(highlighted: nil, query: "nothing like it", in: model.tags))
        XCTAssertEqual(model.tags.count, before)
    }
}
