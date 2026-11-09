// Castellum - KeyBindingOverride: MenuChord display round-trip and
// CommandSpec.applyingOverride's field-preservation rules.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class KeyBindingOverrideTests: XCTestCase {
    // MARK: MenuChord parsing

    func testDisplayRoundTripsForEveryChordInTheLiveRegistry() {
        for spec in CommandRegistry.all {
            guard let chord = spec.menuChord else { continue }
            XCTAssertEqual(MenuChord(display: chord.display), chord, "\(spec.id)")
        }
    }

    func testParsesEachModifierCombination() {
        XCTAssertEqual(MenuChord(display: "K"), MenuChord("k"))
        XCTAssertEqual(MenuChord(display: "⌘K"), MenuChord("k", command: true))
        XCTAssertEqual(MenuChord(display: "⇧⌘K"), MenuChord("k", command: true, shift: true))
        XCTAssertEqual(MenuChord(display: "⌃⌘A"), MenuChord("a", command: true, control: true))
        XCTAssertEqual(MenuChord(display: "⌘⌫"), MenuChord("⌫", command: true))
    }

    func testRejectsEmptyOrMultiCharacterInput() {
        XCTAssertNil(MenuChord(display: ""))
        XCTAssertNil(MenuChord(display: "⌘"))
        XCTAssertNil(MenuChord(display: "⌘Kx"))
    }

    // MARK: CommandSpec.applyingOverride

    func testOverridingMenuChordLeavesEveryOtherFieldUnchanged() {
        let base = CommandRegistry.spec(for: .archive)
        let patched = base.applyingOverride(KeyBindingOverride(menuChord: "⇧⌘E", bareKey: nil))
        XCTAssertEqual(patched.menuChord?.display, "⇧⌘E")
        XCTAssertEqual(patched.bareKey, base.bareKey)
        XCTAssertEqual(patched.title, base.title)
        XCTAssertEqual(patched.notification, base.notification)
        XCTAssertEqual(patched.destructive, base.destructive)
        XCTAssertEqual(patched.menu?.name, base.menu?.name)
        XCTAssertEqual(patched.sheetGroup, base.sheetGroup)
    }

    func testOverridingBareKeyLeavesMenuChordUnchanged() {
        let base = CommandRegistry.spec(for: .archive)
        let patched = base.applyingOverride(KeyBindingOverride(menuChord: nil, bareKey: "y"))
        XCTAssertEqual(patched.bareKey, "y")
        XCTAssertEqual(patched.menuChord, base.menuChord)
    }

    func testUnparseableChordKeepsTheOriginalRatherThanBlanking() {
        let base = CommandRegistry.spec(for: .archive)
        let patched = base.applyingOverride(KeyBindingOverride(menuChord: "not-a-chord", bareKey: nil))
        XCTAssertEqual(patched.menuChord, base.menuChord, "an unparseable override must never blank a binding")
    }

    func testMultiCharacterBareKeyIsIgnored() {
        let base = CommandRegistry.spec(for: .archive)
        let patched = base.applyingOverride(KeyBindingOverride(menuChord: nil, bareKey: "yy"))
        XCTAssertEqual(patched.bareKey, base.bareKey)
    }

    // MARK: JSON decoding

    func testDecodesAMinimalFile() throws {
        let json = """
        {"version": 1, "overrides": {"archive": {"bareKey": "y"}}}
        """.data(using: .utf8)!
        let file = try JSONDecoder().decode(KeyBindingOverridesFile.self, from: json)
        XCTAssertEqual(file.overrides["archive"], KeyBindingOverride(menuChord: nil, bareKey: "y"))
    }

    func testDecodingIgnoresUnknownTopLevelKeys() throws {
        // KeyBindingsStore (Task 6) writes extra documentation keys into the
        // file it generates — decoding a file that has them must not fail.
        let json = """
        {"version": 1, "note": "hello", "overrides": {}}
        """.data(using: .utf8)!
        XCTAssertNoThrow(try JSONDecoder().decode(KeyBindingOverridesFile.self, from: json))
    }

    // MARK: keys (file version 2, MM-12)

    func testKeysReplaceTheBareKeyAndTheGChord() {
        let base = CommandRegistry.spec(for: .gotoInbox)
        XCTAssertEqual(base.sequences.map(\.fileSyntax), ["g i"])
        let patched = base.applyingOverride(KeyBindingOverride(keys: ["⌃x i", "Space"]))
        XCTAssertEqual(patched.sequences.map(\.fileSyntax), ["⌃x i", "Space"])
        XCTAssertEqual(patched.gChord, base.gChord, "the shorthand fields are untouched; sequences win")
        XCTAssertEqual(patched.notification, base.notification)
        XCTAssertEqual(patched.title, base.title)
    }

    func testKeysWinOverBareKeyInTheSameOverride() {
        let patched = CommandRegistry.spec(for: .archive)
            .applyingOverride(KeyBindingOverride(menuChord: nil, bareKey: "w", keys: ["y"]))
        XCTAssertEqual(patched.sequences.map(\.fileSyntax), ["y"])
    }

    func testEmptyKeysUnbinds() {
        XCTAssertEqual(CommandRegistry.spec(for: .archive).applyingOverride(KeyBindingOverride(keys: [])).sequences, [])
    }

    func testAnUnparseableSequenceLeavesTheKeysUnchanged() {
        let base = CommandRegistry.spec(for: .archive)
        let patched = base.applyingOverride(KeyBindingOverride(keys: ["y", "a b c d e"]))
        XCTAssertEqual(patched.sequences, base.sequences)
    }

    func testNeitherKeysNorBareKeyChangesNothing() {
        let base = CommandRegistry.spec(for: .archive)
        XCTAssertEqual(base.applyingOverride(KeyBindingOverride()).sequences, base.sequences)
        XCTAssertNil(base.applyingOverride(KeyBindingOverride()).sequenceOverride)
    }

    func testDecodesKeys() throws {
        let json = #"{"version": 2, "overrides": {"archive": {"keys": ["y", "g i"]}}}"#.data(using: .utf8)!
        let file = try JSONDecoder().decode(KeyBindingOverridesFile.self, from: json)
        XCTAssertEqual(file.version, 2)
        XCTAssertEqual(file.overrides["archive"]?.keys, ["y", "g i"])
    }
}
