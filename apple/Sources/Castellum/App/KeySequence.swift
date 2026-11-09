// Castellum - key sequences (MM-12 slice 1): the one binding model behind every
// bare key. A sequence is 1 to 4 strokes ("e", "g i", "⌃x a"); the hard-coded
// `g …` chord is just the two-stroke sequence `g <key>`. Pure Foundation/AppKit
// types so the parser, the lookup table and the router are unit-testable.
//
// File syntax (one stroke): optional glyphs `⌃ ⌥ ⇧` in any order, then exactly
// one printable character or a named key (Return, Space, ⌫/Delete, ↑ ↓ ← →, also
// Up/Down/Left/Right; names match ignoring case). An uppercase letter means
// ⇧+letter ("R" == "⇧r"). Shifted symbols are written as the character they
// produce ("#", "?") without ⇧. ⌘ is never allowed (it belongs to `menuChord`;
// AppKit hands ⌘ key equivalents to the menus before any view sees them), and
// neither are Esc (cancels a pending sequence) or Tab (focus traversal).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import Foundation

/// One key press: a printable character or a named key, plus ⌃/⌥ (and ⇧ for
/// named keys; for a character ⇧ is folded into the character itself).
struct KeyStroke: Hashable {
    enum NamedKey: String, CaseIterable {
        case returnKey = "Return", space = "Space", delete = "⌫"
        case up = "↑", down = "↓", left = "←", right = "→"
        /// Seen by the router (Esc cancels a pending sequence, Tab is swallowed
        /// after a prefix) but never accepted from a file.
        case escape = "Esc", tab = "Tab"

        /// The keys a user may bind.
        static let bindable: [NamedKey] = allCases.filter { $0 != .escape && $0 != .tab }

        /// Extra spellings accepted from a file, matched ignoring case.
        fileprivate static let aliases: [String: NamedKey] = [
            "delete": .delete, "up": .up, "down": .down, "left": .left, "right": .right,
        ]
    }

    enum Key: Hashable {
        case character(Character)
        case named(NamedKey)
    }

    let key: Key
    let control: Bool
    let option: Bool
    /// Only meaningful for named keys; always false for a character stroke.
    let shift: Bool

    /// Memberwise, normalising: a character stroke folds ⇧ into the character
    /// (letters upper-case; ⇧ on a non-letter is dropped, the character already
    /// is what the key produces), so equal strokes are `==`.
    init(_ key: Key, control: Bool = false, option: Bool = false, shift: Bool = false) {
        switch key {
        case .character(let c):
            let folded = shift ? Self.fold(c) : c
            self.key = .character(folded)
            self.shift = false
        case .named:
            self.key = key
            self.shift = shift
        }
        self.control = control
        self.option = option
    }

    /// A plain character stroke ("e", "R", "#"), as the registry's `bareKey` and
    /// `gChord` fields hold them. Uppercase letters mean ⇧+letter.
    init(character: Character) { self.init(.character(character)) }

    private static func fold(_ c: Character) -> Character {
        let up = String(c).uppercased()
        return up.count == 1 ? Character(up) : c
    }

    /// Parses one stroke in file syntax; nil = not understood or not allowed
    /// (⌘, Esc, Tab, an unknown name, more than one character).
    init?(display raw: String) {
        var control = false, option = false, shift = false
        var rest = Substring(raw.precomposedStringWithCanonicalMapping)
        while let first = rest.first, "⌃⌥⇧⌘".contains(first) {
            switch first {
            case "⌃": control = true
            case "⌥": option = true
            case "⇧": shift = true
            default: return nil // ⌘ belongs to menuChord (K5)
            }
            rest.removeFirst()
        }
        guard !rest.isEmpty else { return nil }
        let word = String(rest)
        if let named = NamedKey.bindable.first(where: { $0.rawValue.lowercased() == word.lowercased() })
            ?? NamedKey.aliases[word.lowercased()] {
            self.init(.named(named), control: control, option: option, shift: shift)
            return
        }
        guard word.count == 1, let c = word.first, Self.isPrintable(c) else { return nil }
        // "⇧" on something that has no upper-case form says nothing a file can
        // mean ("⇧3" would be "#"): refuse rather than guess.
        if shift, Self.fold(c) == c, !c.isUppercase { return nil }
        self.init(.character(c), control: control, option: option, shift: shift)
    }

    /// The stroke an AppKit key-down stands for; nil = not representable (⌘
    /// held, a dead key, a function key). Modifiers are masked to
    /// ⇧⌃⌥⌘ so Caps Lock and the arrows' `.function`/`.numericPad` flags never
    /// leak in; named keys come from key codes (an arrow's characters are
    /// private-use scalars).
    init?(event: NSEvent) {
        let flags = event.modifierFlags.intersection([.shift, .control, .option, .command])
        guard !flags.contains(.command) else { return nil }
        let control = flags.contains(.control), option = flags.contains(.option)
        let shift = flags.contains(.shift)
        let named: NamedKey?
        switch event.keyCode {
        case 36, 76: named = .returnKey
        case 49: named = .space
        case 51: named = .delete
        case 123: named = .left
        case 124: named = .right
        case 125: named = .down
        case 126: named = .up
        case 53: named = .escape
        case 48: named = .tab
        default: named = nil
        }
        if let named {
            self.init(.named(named), control: control, option: option, shift: shift)
            return
        }
        guard let chars = event.charactersIgnoringModifiers?.precomposedStringWithCanonicalMapping,
              chars.count == 1, let c = chars.first, Self.isPrintable(c) else { return nil }
        self.init(.character(c), control: control, option: option, shift: shift)
    }

    private static func isPrintable(_ c: Character) -> Bool {
        c.unicodeScalars.allSatisfy { s in
            switch s.properties.generalCategory {
            case .control, .format, .surrogate, .privateUse, .unassigned, .lineSeparator,
                 .paragraphSeparator, .spaceSeparator:
                return false
            default:
                return true
            }
        }
    }

    var isEscape: Bool { key == .named(.escape) }

    /// Canonical file syntax; `KeyStroke(display:)` parses it back to an equal
    /// stroke ("⌃x", "R", "#", "⇧Space").
    var fileSyntax: String {
        var out = ""
        if control { out += "⌃" }
        if option { out += "⌥" }
        if shift { out += "⇧" }
        switch key {
        case .character(let c): out.append(c)
        case .named(let n): out += n.rawValue
        }
        return out
    }

    /// Cheat-sheet rendering: letters upper-cased, ⇧ shown for an upper-case
    /// letter ("E", "⇧R", "⌃X", "?", "Space").
    var display: String {
        var out = ""
        if control { out += "⌃" }
        if option { out += "⌥" }
        switch key {
        case .character(let c):
            if c.isUppercase { out += "⇧" }
            out += String(c).uppercased()
        case .named(let n):
            if shift { out += "⇧" }
            out += n.rawValue
        }
        return out
    }
}

/// 1 to 4 strokes pressed in turn.
struct KeySequence: Hashable {
    static let maxLength = 4
    let strokes: [KeyStroke]

    init?(strokes: [KeyStroke]) {
        guard (1...Self.maxLength).contains(strokes.count) else { return nil }
        self.strokes = strokes
    }

    /// Parses "g i", "⌃x a": 1 to 4 strokes separated by single spaces. Empty
    /// text, double spaces and a fifth stroke are refused.
    init?(display raw: String) {
        let parts = raw.split(separator: " ", omittingEmptySubsequences: false)
        guard (1...Self.maxLength).contains(parts.count) else { return nil }
        var strokes: [KeyStroke] = []
        for part in parts {
            guard let stroke = KeyStroke(display: String(part)) else { return nil }
            strokes.append(stroke)
        }
        self.strokes = strokes
    }

    /// Canonical file syntax ("g i", "⌃x a"); round-trips through `init(display:)`.
    var fileSyntax: String { strokes.map(\.fileSyntax).joined(separator: " ") }

    /// Cheat-sheet rendering ("G I", "⌃X A").
    var display: String { strokes.map(\.display).joined(separator: " ") }

    /// True when `self` is shorter than `other` and equals its first strokes.
    func isStrictPrefix(of other: KeySequence) -> Bool {
        strokes.count < other.strokes.count && Array(other.strokes.prefix(strokes.count)) == strokes
    }
}

/// Lookup table for the router, built from the effective specs. It is meant
/// to be built from conflict-free specs (`CommandRegistry.problems(in:)` is
/// empty); on a conflicting input the first spec in order wins an exact
/// match, and an exact match always beats a prefix.
struct KeySequenceTable {
    enum Match: Equatable {
        case command(CommandID)
        case prefix
        case none
    }

    /// Changes with every build, so a router can tell a reloaded table from the
    /// one a pending prefix was armed against.
    let id = UUID()
    private var exact: [[KeyStroke]: CommandID] = [:]
    private var prefixes: Set<[KeyStroke]> = []

    init(specs: [CommandSpec]) {
        for spec in specs where CommandRegistry.isBindable(spec) {
            for sequence in spec.sequences {
                if exact[sequence.strokes] == nil { exact[sequence.strokes] = spec.id }
                for n in 1..<sequence.strokes.count { prefixes.insert(Array(sequence.strokes.prefix(n))) }
            }
        }
    }

    func match(_ strokes: [KeyStroke]) -> Match {
        if let id = exact[strokes] { return .command(id) }
        return prefixes.contains(strokes) ? .prefix : .none
    }
}
