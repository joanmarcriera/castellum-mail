// Castellum - command registry invariants: completeness, conflict detection,
// legacy-binding agreement (until the menus migrate onto the registry),
// cheat-sheet derivation, and the destructive-command focus policy.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI
import XCTest
@testable import Castellum

final class CommandRegistryTests: XCTestCase {
    // MARK: Tag key hint

    /// The Tags settings hint advertises the real Tag shortcut. The key is `l` (MailMate
    /// parity: L = label); the hint shows it uppercase because lowercase `l` reads as `I`.
    func testTagsHintNamesTheRealTagKeyUnambiguously() {
        XCTAssertEqual(CommandRegistry.spec(for: .tagMessage).bareKey, "l")
        XCTAssertEqual(CommandRegistry.command(forBareKey: "l")?.id, .tagMessage)
        XCTAssertNil(CommandRegistry.command(forBareKey: "i"), "`I` is not bound; the hint must not suggest it")
        XCTAssertTrue(TagsSettingsTab.emptyStateHint.contains("(press L)"), TagsSettingsTab.emptyStateHint)
    }

    // MARK: Completeness

    func testEveryCommandHasExactlyOneRegistryEntry() {
        let ids = CommandRegistry.all.map(\.id)
        XCTAssertEqual(ids.count, Set(ids).count, "duplicate registry entries")
        XCTAssertEqual(
            Set(ids), Set(CommandID.allCases),
            "every CommandID must appear in the registry exactly once"
        )
    }

    func testEveryMenuCommandDispatchesViaNotification() {
        for spec in CommandRegistry.all where spec.menu != nil {
            XCTAssertNotNil(
                spec.notification,
                "\(spec.id) has a menu placement but no dispatch notification"
            )
        }
    }

    /// FP42: Forward was only a toolbar button — no menu item, chord or
    /// palette entry. It is a registered Message-menu command with ⇧⌘F.
    func testForwardIsARegisteredCommand() throws {
        let id = try XCTUnwrap(CommandID(rawValue: "forward"))
        let spec = CommandRegistry.spec(for: id)
        XCTAssertEqual(spec.notification, .castellumForward)
        XCTAssertEqual(spec.menu?.name, .message)
        XCTAssertEqual(spec.menuChord?.display, "⇧⌘F")
        XCTAssertNil(spec.bareKey, "bare f is Flag")
        let others = CommandRegistry.all.filter { $0.id != id }.compactMap(\.menuChord?.display)
        XCTAssertFalse(others.contains("⇧⌘F"), "Forward's chord must not collide")
    }

    // MARK: Conflict detection

    func testMenuKeyEquivalentsNeverConflict() {
        let chords = CommandRegistry.all.compactMap(\.menuChord)
        let displays = chords.map(\.display)
        XCTAssertEqual(
            displays.count, Set(displays).count,
            "two commands share a menu key equivalent: \(displays.sorted())"
        )
    }

    func testBareListKeysNeverConflictAndReserveTheChordPrefix() {
        let bare = CommandRegistry.all.compactMap(\.bareKey)
        XCTAssertEqual(
            bare.count, Set(bare).count,
            "two commands share a bare message-list key: \(bare.sorted())"
        )
        XCTAssertFalse(
            bare.contains("g"),
            "'g' is the chord prefix and can never be a bare command key"
        )
    }

    func testGChordSecondKeysNeverConflict() {
        let chords = CommandRegistry.all.compactMap(\.gChord)
        XCTAssertEqual(
            chords.count, Set(chords).count,
            "two commands share a g-chord: \(chords.sorted())"
        )
    }

    func testConflictsHelperAgreesWithTheBuiltInTable() {
        XCTAssertEqual(
            CommandRegistry.conflicts(in: CommandRegistry.all), [],
            "the shipped registry must have zero conflicts by construction"
        )
    }

    func testConflictsHelperDetectsAnInjectedDuplicate() {
        var specs = CommandRegistry.all
        // Force Reply All's chord onto Reply too.
        let replyAllChord = CommandRegistry.spec(for: .replyAll).menuChord!
        specs[specs.firstIndex { $0.id == .reply }!] = CommandSpec(
            .reply, title: "Reply", menuChord: replyAllChord, notification: .castellumReply)
        let conflicts = CommandRegistry.conflicts(in: specs)
        XCTAssertTrue(conflicts.contains { $0.contains(replyAllChord.display) })
    }

    // MARK: Menu generation

    func testMenuSectionsMatchTheAcceptedMenuLayout() {
        func titles(_ menu: MenuName) -> [[String]] {
            CommandRegistry.menuSections(for: menu).map { $0.map(\.title) }
        }
        XCTAssertEqual(titles(.app), [["Check for Updates…"]])
        XCTAssertEqual(titles(.file), [["New Message"], ["Import…", "Export as .eml…"], ["Print…"]])
        XCTAssertEqual(titles(.search), [["Search Mail"]])
        XCTAssertEqual(titles(.message), [
            ["Reply", "Reply All", "Forward", "Reply with AI", "Send Later…", "Insert Snippet…"],
            ["Summarize"],
            ["Toggle Read/Unread", "Mark as Read", "Mark as Unread", "Mark All as Read", "Flag / Unflag", "Tag…", "Mute / Unmute Conversation",
             "Add / Remove Sender as VIP", "Snooze…", "Move to Inbox Now"],
            ["Undo Archive or Delete", "Archive", "Delete", "Move to Folder…", "Mark as Junk", "Not Junk", "Empty Trash…"],
            ["Outbox", "Send All Waiting Messages"],
        ])
        XCTAssertEqual(titles(.view), [
            ["Command Palette…", "Fit Columns"], ["Message Source", "Expand All Quotes"],
            ["Widescreen Layout", "Classic Layout"], ["Show Correspondence with Sender"],
            ["Next Split", "Previous Split"],
            ["Compact Headers", "Extended Headers", "All Headers"],
            ["Load Remote Images"],
        ])
        XCTAssertEqual(titles(.window), [["Status", "AI Activity", "Snippets"], ["Main Window"]])
        XCTAssertEqual(titles(.people), [["Contacts"]])
        XCTAssertEqual(titles(.projects), [["Projects Board", "Assign to Project"]])
        XCTAssertEqual(titles(.help), [["Keyboard Shortcuts"], ["Diagnostics"]])
    }

    func testChordToKeyEquivalentMapping() {
        let delete = CommandRegistry.spec(for: .delete).menuChord!
        XCTAssertEqual(delete.keyEquivalent.character, KeyEquivalent.delete.character)
        XCTAssertEqual(delete.eventModifiers, [.command])

        let markAll = CommandRegistry.spec(for: .markAllRead).menuChord!
        XCTAssertEqual(markAll.keyEquivalent.character, "a")
        XCTAssertEqual(markAll.eventModifiers, [.command, .control])

        let assign = CommandRegistry.spec(for: .assignProject).menuChord!
        XCTAssertEqual(assign.keyEquivalent.character, "p")
        XCTAssertEqual(assign.eventModifiers, [], "modifier-less menu equivalent")

        let replyAll = CommandRegistry.spec(for: .replyAll).menuChord!
        XCTAssertEqual(replyAll.keyEquivalent.character, "r")
        XCTAssertEqual(replyAll.eventModifiers, [.command, .shift])
    }

    // MARK: Pinned chord values (the menus render exactly these)

    func testRegistryAgreesWithLegacyMenuDefinitions() {
        func chord(_ id: CommandID) -> String? {
            CommandRegistry.spec(for: id).menuChord?.display
        }
        XCTAssertEqual(chord(.compose), "⌘N")
        XCTAssertEqual(chord(.focusSearch), "⌘K")
        XCTAssertEqual(chord(.reply), "⌘R")
        XCTAssertEqual(chord(.replyAll), "⇧⌘R")
        XCTAssertEqual(chord(.summarize), "⇧⌘S")
        XCTAssertEqual(chord(.toggleRead), "⇧⌘U")
        XCTAssertEqual(chord(.markAllRead), "⌃⌘A")
        XCTAssertEqual(chord(.toggleFlag), "⇧⌘L")
        XCTAssertEqual(chord(.archive), "⌘E")
        XCTAssertEqual(chord(.delete), "⌘⌫")
        XCTAssertEqual(chord(.showContacts), "⇧⌘C")
        XCTAssertEqual(chord(.showProjects), "⇧⌘P")
        XCTAssertEqual(chord(.assignProject), "P", "modifier-less menu equivalent")
        XCTAssertEqual(chord(.showShortcuts), "⌘/")
        XCTAssertEqual(chord(.showCommandPalette), "⇧⌘K")
        for id: CommandID in [
            .emptyTrash, .showOutbox, .fitColumns, .showDiagnostics, .checkForUpdates, .showStatus,
            .showAiActivity,
        ] {
            XCTAssertNil(chord(id), "\(id) has no key equivalent in the menus")
        }
    }

    func testRegistryAgreesWithLegacyListKeysAndChords() {
        func bare(_ id: CommandID) -> Character? {
            CommandRegistry.spec(for: id).bareKey
        }
        XCTAssertEqual(bare(.nextMessage), "j")
        XCTAssertEqual(bare(.previousMessage), "k")
        XCTAssertEqual(bare(.archive), "e")
        XCTAssertEqual(bare(.delete), "#")
        XCTAssertEqual(bare(.toggleRead), "u")
        XCTAssertEqual(bare(.toggleFlag), "f")
        XCTAssertEqual(bare(.compose), "c")
        XCTAssertEqual(bare(.reply), "r")
        XCTAssertEqual(bare(.replyAll), "R", "uppercase = Shift+R")
        XCTAssertEqual(bare(.assignProject), "p")
        XCTAssertEqual(bare(.showShortcuts), "?")

        func chord(_ id: CommandID) -> Character? {
            CommandRegistry.spec(for: id).gChord
        }
        XCTAssertEqual(chord(.gotoInbox), "i")
        XCTAssertEqual(chord(.gotoArchive), "a")
        XCTAssertEqual(chord(.gotoSent), "s")
        XCTAssertEqual(chord(.gotoTrash), "t")
        XCTAssertEqual(chord(.gotoDrafts), "d")
        XCTAssertEqual(chord(.gotoJunk), "j")
        XCTAssertEqual(chord(.showContacts), "c")
        XCTAssertEqual(chord(.showProjects), "p")
    }

    // MARK: Destructive policy

    func testDestructiveSetIsExactlyThePinnedCommands() {
        let destructive = Set(CommandRegistry.all.filter(\.destructive).map(\.id))
        XCTAssertEqual(
            destructive, [.archive, .delete, .emptyTrash, .markAllRead, .markJunk, .markNotJunk],
            "destructive/bulk-mutating commands are a deliberate, pinned set"
        )
    }

    @MainActor
    func testDispatcherBlocksDestructiveCommandsWhileTextEditing() {
        var fired: [String] = []
        let editing = CommandDispatcher(isTextEditing: { true })
        editing.perform(.castellumDelete) { fired.append("delete") }
        editing.perform(.castellumArchive) { fired.append("archive") }
        editing.perform(.castellumEmptyTrash) { fired.append("emptyTrash") }
        editing.perform(.castellumMarkAllRead) { fired.append("markAllRead") }
        XCTAssertTrue(
            fired.isEmpty,
            "destructive commands must not fire while a text editor owns focus"
        )

        editing.perform(.castellumReply) { fired.append("reply") }
        editing.perform(.castellumShowOutbox) { fired.append("outbox") }
        XCTAssertEqual(
            fired, ["reply", "outbox"],
            "non-destructive commands still work while editing text"
        )

        let idle = CommandDispatcher(isTextEditing: { false })
        idle.perform(.castellumDelete) { fired.append("delete") }
        XCTAssertEqual(fired.last, "delete", "destructive commands fire when no editor has focus")
    }

    @MainActor
    func testFocusPolicyClassifiesResponders() {
        XCTAssertTrue(CommandFocusPolicy.isTextEditing(NSTextView()))
        XCTAssertTrue(CommandFocusPolicy.isTextEditing(NSTextField()))
        XCTAssertTrue(
            CommandFocusPolicy.isTextEditing(NSSearchField()),
            "NSSearchField is an NSTextField subclass"
        )
        XCTAssertFalse(CommandFocusPolicy.isTextEditing(NSTableView()))
        XCTAssertFalse(CommandFocusPolicy.isTextEditing(nil))
    }

    // MARK: Cheat-sheet derivation

    func testSheetGroupsKeepTheAcceptedStructure() {
        let groups = CommandRegistry.sheetGroups()
        XCTAssertEqual(
            groups.map(\.title),
            ["Navigate", "Go to mailbox", "Act on selection",
             "Compose", "Projects & people", "Help"]
        )

        func rows(_ title: String) -> [(keys: [String], action: String)] {
            groups.first { $0.title == title }!.items.map { ($0.keys, $0.action) }
        }

        let navigate = rows("Navigate")
        XCTAssertEqual(navigate.first?.keys, ["J", "↓"])
        XCTAssertEqual(navigate.first?.action, "Next message")
        XCTAssertEqual(navigate.last?.keys, ["⌘K"], "Search stays the last Navigate row")

        let goto = rows("Go to mailbox")
        XCTAssertEqual(
            goto.map(\.keys),
            [["G I"], ["G A"], ["G S"], ["G T"], ["G D"], ["G J"], ["G L"], ["G C"], ["G P"]],
            "mailbox chords first (G L is the MM-8 tag list), then the window chords"
        )

        let act = rows("Act on selection")
        XCTAssertTrue(
            act.contains { $0.action == "Archive" && $0.keys == ["E", "⌘E"] },
            "the sheet now shows the menu chord next to the bare key"
        )
        XCTAssertTrue(act.contains { $0.action == "Delete (to Trash)" && $0.keys == ["#", "⌘⌫"] })
        XCTAssertTrue(
            act.contains { $0.action == "Summarize" && $0.keys == ["⇧⌘S"] },
            "Summarize was missing from the hand-maintained sheet"
        )

        let compose = rows("Compose")
        XCTAssertEqual(compose.first?.keys, ["C", "⌘N"])
        XCTAssertTrue(compose.contains { $0.keys == ["⇧R", "⇧⌘R"] && $0.action == "Reply all" })
        XCTAssertEqual(compose.last?.keys, ["⌘⏎"], "Send stays the last Compose row")

        let projects = rows("Projects & people")
        XCTAssertTrue(
            projects.contains { $0.action == "Assign to project" && $0.keys == ["P"] },
            "the bare key and the modifier-less menu equivalent render one keycap"
        )

        XCTAssertEqual(rows("Help").map(\.keys), [["⇧⌘K"], ["?", "⌘/"]])
    }

    func testSendLaterCommandsAreRegisteredAndReachable() {
        for id: CommandID in [
            .sendLater, .sendLaterInOneHour, .sendLaterThisEvening,
            .sendLaterTomorrowMorning, .sendLaterMonday,
        ] {
            let spec = CommandRegistry.spec(for: id)
            XCTAssertNotNil(spec.notification, "\(id) must dispatch (palette, menu, key bindings)")
        }
        XCTAssertEqual(CommandRegistry.spec(for: .sendLater).menuChord?.display, "⌥⌘L")
        let compose = CommandRegistry.sheetGroups().first { $0.title == "Compose" }!.items
        XCTAssertTrue(compose.contains { $0.keys == ["⌥⌘L"] })
    }

    func testSheetOmitsKeylessCommands() {
        let actions = CommandRegistry.sheetGroups().flatMap(\.items).map(\.action)
        for absent in [
            "Empty Trash…", "Outbox", "Fit Columns", "Diagnostics", "Check for Updates…",
            "Reply with AI", "Status", "AI Activity",
        ] {
            XCTAssertFalse(
                actions.contains(absent),
                "\(absent) has no shortcut and stays out of the shortcuts sheet"
            )
        }
    }

    // MARK: MM-12 slice 1: every command is bindable or pinned fixed

    func testEveryCommandIsBindableOrPinnedFixed() {
        let bindable = Set(CommandRegistry.all.filter(CommandRegistry.isBindable).map(\.id))
        XCTAssertEqual(Set(CommandID.allCases).subtracting(bindable), CommandRegistry.fixedBindingCommands)
        XCTAssertEqual(
            CommandRegistry.fixedBindingCommands,
            [.sendComposer, .sendLater, .sendLaterInOneHour, .sendLaterThisEvening,
             .sendLaterTomorrowMorning, .sendLaterMonday, .insertSnippet],
            "sendComposer is the SwiftUI Send shortcut of another window; the Send Later presets act on a compose window")
        for id in [CommandID.nextMessage, .previousMessage, .openMessage, .openInWindow,
                   .gotoInbox, .gotoArchive, .gotoSent, .gotoTrash, .gotoDrafts, .gotoJunk, .gotoTag] {
            XCTAssertTrue(CommandRegistry.isBindable(CommandRegistry.spec(for: id)), "\(id) was fixed before MM-12")
        }
    }

    func testDefaultSequencesAreConflictFree() {
        XCTAssertEqual(CommandRegistry.problems(in: CommandRegistry.all), [])
        let all = CommandRegistry.all.flatMap(\.sequences)
        XCTAssertEqual(all.count, Set(all).count, "no two commands share a default sequence")
        // `g` is a pure prefix: nothing is bound to it alone.
        XCTAssertFalse(all.contains(KeySequence(display: "g")!))
    }

    func testPrefixShadowingIsAConflict() {
        // Inject `g` onto archive: it hides every `g …` sequence.
        let specs = CommandRegistry.all.map {
            $0.id == .archive ? $0.applyingOverride(KeyBindingOverride(keys: ["g"])) : $0
        }
        let problems = CommandRegistry.problems(in: specs).filter { $0.kind == .shadowedByPrefix }
        XCTAssertTrue(problems.contains { $0.commandId == "gotoInbox" }, "\(problems)")
        XCTAssertTrue(problems.allSatisfy { $0.message.contains("G") })
        // The other direction: a long sequence added under an existing one-stroke binding.
        let under = CommandRegistry.all.map {
            $0.id == .snooze ? $0.applyingOverride(KeyBindingOverride(keys: ["e x"])) : $0
        }
        XCTAssertTrue(CommandRegistry.problems(in: under).contains { $0.kind == .shadowedByPrefix && $0.commandId == "snooze" })
    }

    func testDuplicateSequenceIsANamedProblem() {
        let specs = CommandRegistry.all.map {
            $0.id == .reply ? $0.applyingOverride(KeyBindingOverride(keys: ["e"])) : $0
        }
        let dup = CommandRegistry.problems(in: specs).filter { $0.kind == .duplicate }
        XCTAssertEqual(dup.count, 1)
        XCTAssertTrue(dup[0].message.contains("archive") && dup[0].message.contains("reply"), dup[0].message)
    }

    func testMenuChordWithoutMenuItemIsAProblem() {
        let specs = CommandRegistry.all.map {
            $0.id == .nextMessage ? $0.applyingOverride(KeyBindingOverride(menuChord: "⌃⌘J")) : $0
        }
        let problems = CommandRegistry.problems(in: specs)
        XCTAssertEqual(problems.map(\.kind), [.noMenuItem])
        XCTAssertEqual(problems.first?.commandId, "nextMessage")
        // The built-in table itself has such chords nowhere (they all have a menu item).
        XCTAssertTrue(CommandRegistry.all.filter { $0.menu == nil }.allSatisfy { $0.menuChord == nil })
    }

    func testSequenceOnANonBindableCommandIsAProblem() {
        let specs = CommandRegistry.all.map {
            $0.id == .sendComposer ? $0.applyingOverride(KeyBindingOverride(keys: ["y"])) : $0
        }
        XCTAssertEqual(CommandRegistry.problems(in: specs).map(\.kind), [.notBindable])
    }

    func testSheetShowsUserSequencesAndStillShowsDefaultsAsBefore() {
        let specs = CommandRegistry.all.map {
            $0.id == .archive ? $0.applyingOverride(KeyBindingOverride(keys: ["y", "⌃x a"])) : $0
        }
        let groups = CommandRegistry.sheetGroups(from: specs)
        let archive = groups.flatMap(\.items).first { $0.action == "Archive" && $0.keys.contains("Y") }
        XCTAssertNotNil(archive, "single-stroke user keys render as keycaps")
        let goto = groups.first { $0.title == "Go to mailbox" }
        XCTAssertTrue(goto?.items.contains { $0.keys == ["⌃X A"] } == true, "multi-stroke user keys render in the chord group")
        XCTAssertTrue(CommandRegistry.sheetGroups().flatMap(\.items).contains { $0.keys == ["G I"] })
    }
}
