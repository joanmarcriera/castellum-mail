// Castellum - the user key-bindings file format (S45): parsing and applying
// one override, in the exact display syntax the app already shows the user
// ("⇧⌘R", "⌘⌫", a bare "y" or "R" for Shift+R) — see this plan's Assumption
// 8. Pure Swift/Foundation; no file I/O here (KeyBindingsStore owns that).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// One command's override: a field left `nil` keeps that command's built-in
/// default. Scoped to the keys only — not menu placement, title or the
/// destructive flag — see Assumption 7. `keys` (file version 2, MM-12) lists
/// key sequences ("y", "g i", "⌃x a"); when present it REPLACES the command's
/// `bareKey` and g-chord (K4), `[]` unbinds, and `bareKey` in the same entry is
/// ignored with a problem. `bareKey` stays for version 1 files.
struct KeyBindingOverride: Codable, Equatable {
    var menuChord: String?
    var bareKey: String?
    var keys: [String]?
}

/// One thing wrong with `keybindings.json`, named exactly (MM-12). The store
/// publishes these; `warnings` is the sentence list derived from them.
struct KeyBindingProblem: Equatable, Identifiable {
    enum Kind: String {
        /// The file is not JSON of the expected shape.
        case malformedFile
        /// `version` is not one this build reads; the defaults are used.
        case unsupportedVersion
        /// The entry's command id does not exist.
        case unknownCommand
        /// A chord, key or sequence could not be understood.
        case unparseable
        /// Two commands would share a key (or menu chord).
        case duplicate
        /// A shorter sequence would hide a longer one (K7).
        case shadowedByPrefix
        /// A menu chord macOS owns.
        case reservedChord
        /// A key sequence on a command that cannot take one (`sendComposer`, Send Later).
        case notBindable
        /// A menu chord on a command that has no menu item (use `keys`).
        case noMenuItem
        /// A field the file set that another field makes moot (`bareKey` beside `keys`).
        case ignored
    }

    /// The command the problem is about; nil for file-level problems.
    let commandId: String?
    let kind: Kind
    /// A complete, localised sentence.
    let message: String
    var id: String { "\(commandId ?? "-")|\(kind.rawValue)|\(message)" }
}

/// The on-disk shape of `keybindings.json`. `version` is checked against
/// `KeyBindingsStore.supportedVersions` (1 and 2): a file from a later build
/// fails closed with an `unsupportedVersion` problem rather than being
/// silently misread.
struct KeyBindingOverridesFile: Codable, Equatable {
    var version: Int
    var overrides: [String: KeyBindingOverride]
}

extension MenuChord {
    /// Parses a chord back from its own `display` rendering — the exact
    /// string every menu and the cheat sheet already show the user, so
    /// there is nothing new to learn. Leading modifier glyphs (any of
    /// `⌃⌥⇧⌘`, in any order — lenient, since this is a hand-edited file) are
    /// consumed first; what remains must be exactly one character (or the
    /// delete glyph `⌫`). Anything else — empty, multi-character, an
    /// unrecognised glyph mixed in — fails (`nil`) rather than guesses.
    init?(display: String) {
        var remaining = Substring(display)
        var control = false, option = false, shift = false, command = false
        let glyphs: [(Character, () -> Void)] = [
            ("⌃", { control = true }), ("⌥", { option = true }),
            ("⇧", { shift = true }), ("⌘", { command = true }),
        ]
        while let first = remaining.first, let hit = glyphs.first(where: { $0.0 == first }) {
            hit.1()
            remaining.removeFirst()
        }
        let key = String(remaining)
        guard key.count == 1 else { return nil }
        self.init(key == "⌫" ? "⌫" : key.lowercased(),
                  command: command, shift: shift, control: control, option: option)
    }
}

extension CommandSpec {
    /// A copy with `menuChord`/`bareKey`/`keys` replaced by the override's
    /// parsed values; a field the override leaves `nil`, or whose value fails
    /// to parse, keeps THIS spec's own value unchanged (never silently blanks
    /// a binding). `keys` (when every sequence parses) replaces both the bare
    /// key and the g-chord, and wins over a `bareKey` in the same override
    /// (K4). Every other field (`id`, `title`, `menu`, `notification`,
    /// `destructive`, `sheetGroup`, `extraKeys`, `help`) is untouched — see
    /// Assumption 7.
    func applyingOverride(_ override: KeyBindingOverride) -> CommandSpec {
        var resolvedChord = menuChord
        if let raw = override.menuChord {
            resolvedChord = MenuChord(display: raw) ?? menuChord
        }
        var resolvedBareKey = bareKey
        if let raw = override.bareKey, raw.count == 1 {
            resolvedBareKey = raw.first
        }
        var resolvedSequences = sequenceOverride
        if let raw = override.keys {
            let parsed = raw.compactMap { KeySequence(display: $0) }
            if parsed.count == raw.count { resolvedSequences = parsed }
        }
        return CommandSpec(
            id, title: title, help: help, menu: menu, menuChord: resolvedChord,
            bareKey: resolvedBareKey, gChord: gChord, extraKeys: extraKeys,
            notification: notification, destructive: destructive, sheetGroup: sheetGroup,
            sequenceOverride: resolvedSequences)
    }
}
