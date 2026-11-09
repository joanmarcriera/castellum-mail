// Castellum - key-sequence dispatch: the window-level router that makes bare mail
// keys (e/#/u/f/c/r/R/p/? , j/k and the g-chords) work anywhere in the main window,
// routed through the CommandRegistry so destructive commands stay behind the
// CommandDispatcher focus guard.: bare `e` only worked while the
// message list was first responder, and bypassed the guard entirely.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class BareKeyDispatchTests: XCTestCase {
    // MARK: Registry lookup (matching is driven by the registry, no key list)

    func testBareKeyLookupResolvesNotificationCommands() {
        func command(_ key: Character) -> CommandID? {
            CommandRegistry.command(forBareKey: key)?.id
        }
        XCTAssertEqual(command("e"), .archive)
        XCTAssertEqual(command("#"), .delete)
        XCTAssertEqual(command("u"), .toggleRead)
        XCTAssertEqual(command("f"), .toggleFlag)
        XCTAssertEqual(command("c"), .compose)
        XCTAssertEqual(command("r"), .reply)
        XCTAssertEqual(command("R"), .replyAll, "uppercase = Shift+R")
        XCTAssertEqual(command("p"), .assignProject)
        XCTAssertEqual(command("?"), .showShortcuts)
        XCTAssertEqual(
            CommandRegistry.command(forBareKey: "e")?.notification, .castellumArchive,
            "the router posts the command's own notification"
        )
    }

    func testGChordLookupResolvesRegistryCommands() {
        XCTAssertEqual(CommandRegistry.command(forGChord: "i")?.id, .gotoInbox)
        XCTAssertEqual(CommandRegistry.command(forGChord: "c")?.id, .showContacts)
        XCTAssertNil(CommandRegistry.command(forGChord: "x"))
    }

    func testMailboxRolesForGotoCommands() {
        XCTAssertEqual(CommandID.gotoInbox.mailboxRole, "inbox")
        XCTAssertEqual(CommandID.gotoArchive.mailboxRole, "archive")
        XCTAssertEqual(CommandID.gotoSent.mailboxRole, "sent")
        XCTAssertEqual(CommandID.gotoTrash.mailboxRole, "trash")
        XCTAssertEqual(CommandID.gotoDrafts.mailboxRole, "drafts")
        XCTAssertEqual(CommandID.gotoJunk.mailboxRole, "junk")
        XCTAssertNil(CommandID.archive.mailboxRole)
        XCTAssertNil(CommandID.showContacts.mailboxRole, "g c opens a window, not a mailbox")
    }

    // MARK: No double-fire: the list has no local bare keys any more (MM-12)

    func testListHasNoLocalBareKeys() {
        // Every command with a bare key is dispatched by the router through its own
        // notification, j/k included; the list view keeps only the movement.
        for spec in CommandRegistry.all {
            guard spec.bareKey != nil else { continue }
            XCTAssertNotNil(spec.notification, "\(spec.id): a bare key without a notification would be dead")
        }
        XCTAssertEqual(CommandRegistry.spec(for: .nextMessage).notification, .castellumNextMessage)
        XCTAssertEqual(CommandRegistry.spec(for: .previousMessage).notification, .castellumPreviousMessage)
        XCTAssertEqual(CommandRegistry.spec(for: .openMessage).notification, .castellumOpenMessage)
    }

    // MARK: Router behavior

    @MainActor
    private final class Box { var posted: [Notification.Name] = [] }

    @MainActor
    private func makeRouter(
        textEditing: Bool = false, specs: [CommandSpec] = CommandRegistry.all
    ) -> (router: KeySequenceRouter, posted: () -> [Notification.Name]) {
        let box = Box()
        let router = KeySequenceRouter(
            specs: specs, isTextEditing: { textEditing }, post: { box.posted.append($0) })
        return (router, { box.posted })
    }

    @MainActor
    func testRouterSeesReloadedTable() {
        // F5: the router reads the live table on every key, so Reload Key Bindings
        // reaches bare keys without a relaunch. Same router, new bindings.
        final class State { var specs = CommandRegistry.all; var posted: [Notification.Name] = [] }
        let state = State()
        var table = KeySequenceTable(specs: state.specs)
        let router = KeySequenceRouter(
            table: { table }, specs: { state.specs }, isTextEditing: { false },
            post: { state.posted.append($0) })
        XCTAssertEqual(router.press("e"), .consumed)
        // The user rebinds archive e -> a and reloads: no router rebuild.
        state.specs = state.specs.map { $0.id == .archive ? $0.applyingOverride(KeyBindingOverride(bareKey: "a")) : $0 }
        table = KeySequenceTable(specs: state.specs)
        state.posted = []
        XCTAssertEqual(router.press("a"), .consumed)
        XCTAssertEqual(state.posted, [.castellumArchive])
        XCTAssertEqual(router.press("e"), .passthrough)
    }

    @MainActor
    func testJPostsNextMessageExactlyOnce() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.press("j"), .consumed)
        XCTAssertEqual(router.press("k"), .consumed)
        XCTAssertEqual(posted(), [.castellumNextMessage, .castellumPreviousMessage])
    }

    @MainActor
    func testNextMessageRebindFollowsTheLiveBindings() {
        let specs = CommandRegistry.all.map {
            $0.id == .nextMessage ? $0.applyingOverride(KeyBindingOverride(bareKey: "n")) : $0
        }
        let (router, posted) = makeRouter(specs: specs)
        XCTAssertEqual(router.press("n"), .consumed)
        XCTAssertEqual(router.press("j"), .passthrough, "the freed key is no longer navigation")
        XCTAssertEqual(posted(), [.castellumNextMessage])
    }

    @MainActor
    func testBareArchiveKeyPostsExactlyOneArchiveNotification() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.press("e"), .consumed)
        XCTAssertEqual(posted(), [.castellumArchive], "exactly one dispatch, via the registry")
    }

    @MainActor
    func testEveryRegistryBareKeyDispatchesItsOwnNotification() {
        for spec in CommandRegistry.all {
            guard let key = spec.bareKey, let note = spec.notification else { continue }
            let (router, posted) = makeRouter()
            XCTAssertEqual(router.press(String(key)), .consumed, "\(spec.id): bare key must be consumed")
            XCTAssertEqual(posted(), [note], "\(spec.id): posts its registry notification")
        }
    }

    @MainActor
    func testRouterIgnoresKeysWhileTextEditing() {
        let (router, posted) = makeRouter(textEditing: true)
        for key in ["e", "#", "u", "f", "c", "r", "R", "p", "?", "g", "j", "⌃x"] {
            XCTAssertEqual(
                router.press(key), .passthrough,
                "typing '\(key)' in a text field must insert the character, not run a command"
            )
        }
        XCTAssertEqual(posted(), [])
    }

    @MainActor
    func testRouterIgnoresCommandChords() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(
            router.press("e", hasCommand: true), .passthrough,
            "⌘E belongs to the menu key-equivalent path, not the key router"
        )
        XCTAssertEqual(posted(), [])
    }

    @MainActor
    func testRouterIgnoresUnboundKeys() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.press("q"), .passthrough)
        XCTAssertEqual(router.press("Space"), .passthrough)
        XCTAssertEqual(router.press("⌃e"), .passthrough, "an unbound ⌃ stroke is not swallowed")
        XCTAssertEqual(router.handle(stroke: nil, hasCommand: false), .passthrough)
        XCTAssertEqual(posted(), [])
    }

    /// Bare `z` undoes the last archive / delete / move.
    @MainActor
    func testBareZPostsUndoRemoval() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.press("z"), .consumed)
        XCTAssertEqual(posted(), [.castellumUndoRemoval])
    }

    @MainActor
    func testGChordJumpsToMailboxViaItsNotification() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.press("g"), .consumed)
        XCTAssertEqual(router.pendingDisplay, "G")
        XCTAssertEqual(router.press("i"), .consumed)
        XCTAssertNil(router.pendingDisplay)
        XCTAssertEqual(posted(), [.castellumGotoInbox])
    }

    @MainActor
    func testEveryGotoChordPostsItsNotification() {
        let expected: [(Character, Notification.Name)] = [
            ("i", .castellumGotoInbox), ("a", .castellumGotoArchive), ("s", .castellumGotoSent),
            ("t", .castellumGotoTrash), ("d", .castellumGotoDrafts), ("j", .castellumGotoJunk),
            ("l", .castellumGotoTag), ("c", .castellumShowContacts), ("p", .castellumShowProjects),
        ]
        for (key, name) in expected {
            let (router, posted) = makeRouter()
            XCTAssertEqual(router.press("g"), .consumed)
            XCTAssertEqual(router.press(String(key)), .consumed)
            XCTAssertEqual(posted(), [name], "g \(key)")
        }
    }

    @MainActor
    func testGChordOpensContactsAndProjectsViaNotifications() {
        let (router, posted) = makeRouter()
        _ = router.press("g"); _ = router.press("c")
        _ = router.press("g"); _ = router.press("p")
        XCTAssertEqual(posted(), [.castellumShowContacts, .castellumShowProjects])
    }

    @MainActor
    func testUnmatchedKeyAfterAPrefixIsSwallowedAndResets() {
        // K8: `g x` must not leak `x` to the list, and the sequence is over.
        let (router, posted) = makeRouter()
        _ = router.press("g")
        XCTAssertEqual(router.press("x"), .consumed, "an unmatched second key is swallowed")
        XCTAssertNil(router.pendingDisplay)
        XCTAssertEqual(router.press("c"), .consumed, "'c' is compose again, not go-to-contacts")
        XCTAssertEqual(posted(), [.castellumCompose])
    }

    @MainActor
    func testEscCancelsAPendingSequenceAndIsOtherwiseNotOurs() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.handle(stroke: KeyStroke(.named(.escape)), hasCommand: false), .passthrough,
                       "Esc with nothing pending closes sheets and popovers as usual")
        _ = router.press("g")
        XCTAssertEqual(router.handle(stroke: KeyStroke(.named(.escape)), hasCommand: false), .consumed)
        XCTAssertNil(router.pendingDisplay)
        XCTAssertEqual(router.press("i"), .passthrough, "the cancelled prefix does not turn 'i' into a jump")
        XCTAssertEqual(posted(), [])
    }

    @MainActor
    func testThreeStrokeSequenceFiresOnTheThirdStroke() {
        let specs = CommandRegistry.all.map {
            $0.id == .snooze ? $0.applyingOverride(KeyBindingOverride(keys: ["⌃x a b"])) : $0
        }
        let (router, posted) = makeRouter(specs: specs)
        XCTAssertEqual(router.press("⌃x"), .consumed)
        XCTAssertEqual(router.pendingDisplay, "⌃X")
        XCTAssertEqual(router.press("a"), .consumed)
        XCTAssertEqual(router.pendingDisplay, "⌃X A")
        XCTAssertEqual(posted(), [])
        XCTAssertEqual(router.press("b"), .consumed)
        XCTAssertEqual(posted(), [.castellumSnooze])
        XCTAssertEqual(router.press("b"), .passthrough, "the old 'b' binding was replaced by keys (K4)")
    }

    @MainActor
    func testKeysReplaceBothTheBareKeyAndTheGChord() {
        let specs = CommandRegistry.all.map {
            $0.id == .gotoSent ? $0.applyingOverride(KeyBindingOverride(keys: ["⌃x s"])) : $0
        }
        let (router, posted) = makeRouter(specs: specs)
        _ = router.press("g")
        XCTAssertEqual(router.press("s"), .consumed, "the swallowed leftover of the old g s")
        XCTAssertEqual(posted(), [])
        _ = router.press("⌃x"); _ = router.press("s")
        XCTAssertEqual(posted(), [.castellumGotoSent])
    }

    @MainActor
    func testPendingPrefixIsAbandonedWhenTextEditingBegins() {
        final class Flag { var editing = false }
        let flag = Flag()
        var posted: [Notification.Name] = []
        let router = KeySequenceRouter(isTextEditing: { flag.editing }, post: { posted.append($0) })
        _ = router.press("g")
        flag.editing = true
        XCTAssertEqual(router.press("a"), .passthrough)
        flag.editing = false
        XCTAssertEqual(
            router.press("a"), .passthrough,
            "the stale prefix must not fire a mailbox jump after editing intervened"
        )
        XCTAssertEqual(posted, [])
    }

    @MainActor
    func testCancelPendingAbandonsTheSequence() {
        // The monitor calls this when the window resigns key (press `g`, click to
        // another window, come back): the next key must be read fresh.
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.press("g"), .consumed)
        router.cancelPending()
        XCTAssertEqual(router.press("i"), .passthrough,
                       "a cancelled prefix must not turn a later bare 'i' into a mailbox jump")
        XCTAssertEqual(router.press("c"), .consumed)
        XCTAssertEqual(posted(), [.castellumCompose])
        router.cancelPending() // idempotent
        XCTAssertEqual(router.press("q"), .passthrough)
    }

    @MainActor
    func testPendingPrefixExpiresOnItsOwn() {
        // The safety net within the same window: an abandoned `g` clears itself.
        let box = Box()
        let router = KeySequenceRouter(isTextEditing: { false }, post: { box.posted.append($0) }, timeout: 0.1)
        XCTAssertEqual(router.press("g"), .consumed)
        let expired = expectation(description: "prefix expiry elapsed")
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { expired.fulfill() }
        wait(for: [expired], timeout: 3)
        XCTAssertNil(router.pendingDisplay)
        XCTAssertEqual(router.press("i"), .passthrough,
                       "after the timeout, 'i' is a plain key again, not a second stroke")
        XCTAssertEqual(box.posted, [])
    }

    @MainActor
    func testPrefixDoesNotExpireBeforeTheTimeout() {
        let box = Box()
        let router = KeySequenceRouter(isTextEditing: { false }, post: { box.posted.append($0) }, timeout: 5)
        _ = router.press("g")
        XCTAssertEqual(router.press("i"), .consumed)
        XCTAssertEqual(box.posted, [.castellumGotoInbox])
    }

    @MainActor
    func testHoldingAPrefixKeyNeitherFiresNorReArms() {
        // Critic note 8: auto-repeat of `g` must not turn into `g g` (consume + reset) nor re-arm.
        let (router, posted) = makeRouter()
        let g = KeyStroke(display: "g")
        XCTAssertEqual(router.handle(stroke: g, hasCommand: false), .consumed)
        XCTAssertEqual(router.handle(stroke: g, hasCommand: false, isRepeat: true), .consumed)
        XCTAssertEqual(router.pendingDisplay, "G", "a repeat leaves the pending prefix untouched")
        XCTAssertEqual(router.press("i"), .consumed)
        XCTAssertEqual(posted(), [.castellumGotoInbox])
        // A completed single-stroke command still repeats (hold j to scroll the list).
        let j = KeyStroke(display: "j")
        _ = router.handle(stroke: j, hasCommand: false)
        _ = router.handle(stroke: j, hasCommand: false, isRepeat: true)
        XCTAssertEqual(posted().suffix(2), [.castellumNextMessage, .castellumNextMessage])
    }

    @MainActor
    func testReloadWhileAPrefixIsPendingDropsIt() {
        // Critic note 10: a prefix armed against the old table never completes against the new one.
        var table = KeySequenceTable(specs: CommandRegistry.all)
        var posted: [Notification.Name] = []
        let router = KeySequenceRouter(
            table: { table }, specs: { CommandRegistry.all }, isTextEditing: { false },
            post: { posted.append($0) })
        _ = router.press("g")
        table = KeySequenceTable(specs: CommandRegistry.all) // a reload: same bindings, new table
        XCTAssertEqual(router.press("i"), .passthrough, "treated as a fresh 'i'")
        XCTAssertEqual(posted, [])
    }

    @MainActor
    func testReturnRebindWinsOverOpenMessage() {
        // Critic note 14: the monitor runs before KeyTableView.keyDown, so a user binding
        // of Return to another command wins over opening the message. Intended, now pinned.
        let specs = CommandRegistry.all.map {
            $0.id == .archive ? $0.applyingOverride(KeyBindingOverride(keys: ["Return"])) : $0
        }
        let (router, posted) = makeRouter(specs: specs)
        XCTAssertEqual(router.press("Return"), .consumed)
        XCTAssertEqual(posted(), [.castellumArchive])
    }

    @MainActor
    func testAnUnboundReturnPassesThroughToTheTable() {
        let (router, posted) = makeRouter()
        XCTAssertEqual(router.press("Return"), .passthrough)
        XCTAssertEqual(posted(), [])
    }

    @MainActor
    func testDisabledCommandPassesThroughUnlessAPrefixWasPending() {
        let box = Box()
        let router = KeySequenceRouter(
            isTextEditing: { false }, isEnabled: { _ in false }, post: { box.posted.append($0) })
        XCTAssertEqual(router.press("1"), .passthrough, "an empty tag slot is not swallowed (MM-8)")
        _ = router.press("g")
        XCTAssertEqual(router.press("i"), .consumed, "the prefix was already consumed; nothing leaks")
        XCTAssertEqual(box.posted, [])
    }

    // MARK: End-to-end: router → dispatcher guard (the PR #73 policy holds)

    /// Simulates RootView's wiring: router posts → CommandDispatcher.perform.
    @MainActor
    private func makeRouterDispatching(
        into dispatcher: CommandDispatcher
    ) -> (router: KeySequenceRouter, fired: () -> [Notification.Name]) {
        final class Fired { var names: [Notification.Name] = [] }
        let box = Fired()
        let router = KeySequenceRouter(
            isTextEditing: { false }, // router-side focus check passes; dispatcher decides
            post: { name in dispatcher.perform(name) { box.names.append(name) } })
        return (router, { box.names })
    }

    @MainActor
    func testDestructiveBareKeyIsBlockedByDispatcherWhenTextEditing() {
        // Belt and braces: even if a destructive notification reaches the
        // dispatcher while an editor owns focus (races, programmatic posts),
        // the guard still blocks it — bare keys share the menu-chord path.
        let dispatcher = CommandDispatcher(isTextEditing: { true })
        let (router, fired) = makeRouterDispatching(into: dispatcher)
        _ = router.press("e")
        _ = router.press("#")
        XCTAssertEqual(fired(), [], "destructive bare keys never act while text editing")
    }

    @MainActor
    func testDestructiveBareKeyFiresOnceThroughDispatcherWhenNotEditing() {
        let dispatcher = CommandDispatcher(isTextEditing: { false })
        let (router, fired) = makeRouterDispatching(into: dispatcher)
        XCTAssertEqual(router.press("e"), .consumed)
        XCTAssertEqual(fired(), [.castellumArchive], "exactly once, guard passed")
    }

    // MARK: S45 — overridden specs actually change dispatch

    @MainActor
    func testRouterUsesOverriddenSpecsWhenGiven() {
        // Archive's bare key moved to 'y'.
        var overridden = CommandRegistry.all
        let i = overridden.firstIndex { $0.id == .archive }!
        overridden[i] = overridden[i].applyingOverride(KeyBindingOverride(menuChord: nil, bareKey: "y"))
        let (router, posted) = makeRouter(specs: overridden)
        XCTAssertEqual(router.press("e"), .passthrough,
                       "the old 'e' binding no longer dispatches Archive once overridden")
        XCTAssertEqual(router.press("y"), .consumed)
        XCTAssertEqual(posted(), [.castellumArchive])
    }
}
