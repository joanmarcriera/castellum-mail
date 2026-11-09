// Castellum - key sequences (MM-12 slice 1): parsing, display, the lookup table,
// and the AppKit event conversion. Pure; no UI.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

extension KeySequenceRouter {
    /// Test shorthand: press one stroke written in file syntax ("e", "R", "⌃x", "Space").
    @MainActor
    func press(_ stroke: String, hasCommand: Bool = false) -> Outcome {
        handle(stroke: KeyStroke(display: stroke), hasCommand: hasCommand)
    }
}

final class KeySequenceTests: XCTestCase {
    // MARK: Parsing

    func testEveryBindableNamedKeyRoundTripsInEveryModifierOrder() {
        let orders = ["", "⌃", "⌥", "⇧", "⌃⌥", "⌥⌃", "⌃⇧", "⇧⌃", "⌥⇧", "⌃⌥⇧", "⇧⌥⌃"]
        for named in KeyStroke.NamedKey.bindable {
            for mods in orders {
                let text = mods + named.rawValue
                guard let stroke = KeyStroke(display: text) else { XCTFail("\(text) should parse"); continue }
                XCTAssertEqual(stroke.key, .named(named))
                XCTAssertEqual(KeyStroke(display: stroke.fileSyntax), stroke, "\(text) round trip")
                XCTAssertEqual(stroke.control, mods.contains("⌃"))
                XCTAssertEqual(stroke.option, mods.contains("⌥"))
                XCTAssertEqual(stroke.shift, mods.contains("⇧"))
            }
        }
    }

    func testNamedKeysMatchIgnoringCaseAndAcceptTheEnglishAliases() {
        XCTAssertEqual(KeyStroke(display: "return"), KeyStroke(display: "Return"))
        XCTAssertEqual(KeyStroke(display: "SPACE"), KeyStroke(display: "Space"))
        XCTAssertEqual(KeyStroke(display: "Delete"), KeyStroke(display: "⌫"))
        XCTAssertEqual(KeyStroke(display: "Up"), KeyStroke(display: "↑"))
        XCTAssertEqual(KeyStroke(display: "down"), KeyStroke(display: "↓"))
        XCTAssertEqual(KeyStroke(display: "LEFT"), KeyStroke(display: "←"))
        XCTAssertEqual(KeyStroke(display: "right"), KeyStroke(display: "→"))
    }

    func testUppercaseLetterMeansShiftAndEqualsExplicitShift() {
        XCTAssertEqual(KeyStroke(display: "R"), KeyStroke(display: "⇧r"))
        XCTAssertEqual(KeyStroke(display: "R"), KeyStroke(character: "R"))
        XCTAssertNotEqual(KeyStroke(display: "R"), KeyStroke(display: "r"))
        XCTAssertEqual(KeyStroke(display: "R")?.display, "⇧R")
        XCTAssertEqual(KeyStroke(display: "e")?.display, "E")
        XCTAssertEqual(KeyStroke(display: "⌃x")?.display, "⌃X")
        XCTAssertEqual(KeyStroke(display: "#")?.display, "#")
    }

    func testShiftedSymbolsAreWrittenAsTheCharacterNotWithShift() {
        XCTAssertNotNil(KeyStroke(display: "#"))
        XCTAssertNotNil(KeyStroke(display: "?"))
        XCTAssertNil(KeyStroke(display: "⇧3"), "⇧ on a character with no upper case says nothing a file can mean")
    }

    func testCommandIsRefusedAnywhere() {
        XCTAssertNil(KeyStroke(display: "⌘k"))
        XCTAssertNil(KeyStroke(display: "⌃⌘k"))
        XCTAssertNil(KeyStroke(display: "⇧⌘Return"))
        XCTAssertNil(KeySequence(display: "g ⌘i"))
    }

    func testEscAndTabCannotBeBound() {
        for text in ["Esc", "esc", "Escape", "Tab", "⌃Tab", "⇧Esc"] {
            XCTAssertNil(KeyStroke(display: text), text)
        }
    }

    func testMalformedStrokesAreRefused() {
        for text in ["", "⌃", "oo", "cmd+k", "Enter", "⌃⌥", "\u{1B}", "spacebar"] {
            XCTAssertNil(KeyStroke(display: text), "'\(text)'")
        }
        XCTAssertNotNil(KeyStroke(display: "ç"))
        XCTAssertNotNil(KeyStroke(display: "·"))
    }

    func testDecomposedCharactersNormaliseToTheSameStroke() {
        // "é" as e + combining acute (NFD) must equal the precomposed one.
        XCTAssertEqual(KeyStroke(display: "e\u{301}"), KeyStroke(display: "\u{E9}"))
    }

    func testSequencesOfOneToFourStrokes() {
        XCTAssertEqual(KeySequence(display: "e")?.strokes.count, 1)
        XCTAssertEqual(KeySequence(display: "g i")?.strokes.count, 2)
        XCTAssertEqual(KeySequence(display: "⌃x a Return")?.strokes.count, 3)
        XCTAssertEqual(KeySequence(display: "a b c d")?.strokes.count, 4)
        XCTAssertNil(KeySequence(display: "a b c d e"), "five strokes are refused")
        XCTAssertNil(KeySequence(display: "g  i"), "double spaces are refused")
        XCTAssertNil(KeySequence(display: " g"))
        XCTAssertNil(KeySequence(display: "g "))
        XCTAssertNil(KeySequence(display: ""))
        XCTAssertNil(KeySequence(strokes: []))
    }

    func testSequenceRoundTripAndDisplay() {
        let seq = KeySequence(display: "⌃x a")!
        XCTAssertEqual(seq.fileSyntax, "⌃x a")
        XCTAssertEqual(KeySequence(display: seq.fileSyntax), seq)
        XCTAssertEqual(seq.display, "⌃X A")
        XCTAssertEqual(KeySequence(display: "g i")?.display, "G I")
        XCTAssertEqual(KeySequence(display: "g Space")?.display, "G Space")
    }

    func testStrictPrefix() {
        let g = KeySequence(display: "g")!, gi = KeySequence(display: "g i")!, gig = KeySequence(display: "g i g")!
        XCTAssertTrue(g.isStrictPrefix(of: gi))
        XCTAssertTrue(g.isStrictPrefix(of: gig))
        XCTAssertTrue(gi.isStrictPrefix(of: gig))
        XCTAssertFalse(gi.isStrictPrefix(of: g))
        XCTAssertFalse(g.isStrictPrefix(of: g), "equal is a duplicate, not a prefix")
        XCTAssertFalse(KeySequence(display: "h")!.isStrictPrefix(of: gi))
    }

    // MARK: Table

    func testTableMatchReturnsCommandPrefixOrNone() {
        let table = KeySequenceTable(specs: CommandRegistry.all)
        func strokes(_ text: String) -> [KeyStroke] { KeySequence(display: text)!.strokes }
        XCTAssertEqual(table.match(strokes("e")), .command(.archive))
        XCTAssertEqual(table.match(strokes("g i")), .command(.gotoInbox))
        XCTAssertEqual(table.match(strokes("g")), .prefix)
        XCTAssertEqual(table.match(strokes("g x")), .none)
        XCTAssertEqual(table.match(strokes("x")), .none)
        XCTAssertEqual(table.match([]), .none)
        XCTAssertEqual(table.match(strokes("R")), .command(.replyAll))
        XCTAssertEqual(table.match(strokes("1")), .command(.tagSlot1))
    }

    func testTableIgnoresCommandsNoSequenceCanRun() {
        // A Send Later preset with a key (a hand-built spec) never reaches the table.
        var specs = CommandRegistry.all
        let i = specs.firstIndex { $0.id == .sendLaterMonday }!
        specs[i] = specs[i].applyingOverride(KeyBindingOverride(bareKey: "x"))
        XCTAssertEqual(KeySequenceTable(specs: specs).match(KeySequence(display: "x")!.strokes), .none)
    }

    func testEachTableBuildHasItsOwnId() {
        XCTAssertNotEqual(KeySequenceTable(specs: []).id, KeySequenceTable(specs: []).id)
    }

    // MARK: NSEvent conversion

    private func event(_ chars: String, code: UInt16 = 0, flags: NSEvent.ModifierFlags = []) -> NSEvent {
        NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
                         windowNumber: 0, context: nil, characters: chars,
                         charactersIgnoringModifiers: chars, isARepeat: false, keyCode: code)!
    }

    func testEventConversionMasksModifiersAndUsesKeyCodesForNamedKeys() {
        XCTAssertEqual(KeyStroke(event: event("e")), KeyStroke(display: "e"))
        XCTAssertEqual(KeyStroke(event: event("R", flags: .shift)), KeyStroke(display: "R"))
        XCTAssertEqual(KeyStroke(event: event("e", flags: .capsLock)), KeyStroke(display: "e"),
                       "Caps Lock never leaks into a stroke")
        XCTAssertEqual(KeyStroke(event: event("x", flags: .control)), KeyStroke(display: "⌃x"))
        // An arrow carries .function and .numericPad, and its characters are a private-use scalar.
        XCTAssertEqual(KeyStroke(event: event("\u{F701}", code: 125, flags: [.function, .numericPad])),
                       KeyStroke(display: "↓"))
        XCTAssertEqual(KeyStroke(event: event("\u{F700}", code: 126, flags: [.function, .numericPad, .shift])),
                       KeyStroke(display: "⇧↑"))
        XCTAssertEqual(KeyStroke(event: event("\r", code: 36)), KeyStroke(display: "Return"))
        XCTAssertEqual(KeyStroke(event: event("\r", code: 76)), KeyStroke(display: "Return"), "keypad Enter")
        XCTAssertEqual(KeyStroke(event: event(" ", code: 49)), KeyStroke(display: "Space"))
        XCTAssertEqual(KeyStroke(event: event("\u{7F}", code: 51)), KeyStroke(display: "⌫"))
        XCTAssertEqual(KeyStroke(event: event("\u{1B}", code: 53))?.isEscape, true)
    }

    func testEventConversionRefusesCommandAndUnprintableKeys() {
        XCTAssertNil(KeyStroke(event: event("k", flags: .command)))
        XCTAssertNil(KeyStroke(event: event("")), "a dead key produces no character")
        XCTAssertNil(KeyStroke(event: event("\u{F704}", code: 122)), "a function key is not a stroke")
    }

    func testDefaultSequencesComeFromTheBareKeyAndGChordShorthand() {
        XCTAssertEqual(CommandRegistry.spec(for: .archive).sequences.map(\.fileSyntax), ["e"])
        XCTAssertEqual(CommandRegistry.spec(for: .gotoInbox).sequences.map(\.fileSyntax), ["g i"])
        XCTAssertEqual(CommandRegistry.spec(for: .showContacts).sequences.map(\.fileSyntax), ["g c"])
        XCTAssertEqual(CommandRegistry.spec(for: .replyAll).sequences.map(\.fileSyntax), ["R"])
        XCTAssertEqual(CommandRegistry.spec(for: .sendComposer).sequences, [])
    }
}
