// Castellum - reads an optional keybindings.json from the profile directory
// and produces the effective [CommandSpec] table (S45): registry defaults
// with valid, non-conflicting user overrides applied. Untrusted, hand-edited
// input: a missing file means "use every default"; a malformed one falls
// back to the same defaults plus a warning — the same never-crash-on-bad-
// input discipline CLAUDE.md's gotcha #1 requires of IMAP wire input,
// applied here to a different untrusted source.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import Foundation

@MainActor
final class KeyBindingsStore: ObservableObject {
    static let fileName = "keybindings.json"
    /// File versions this build reads. 1 = menuChord/bareKey only; 2 adds
    /// `keys`. Anything else loads the defaults plus an `unsupportedVersion`
    /// problem (K3) instead of being applied as if it were version 1.
    static let supportedVersions: ClosedRange<Int> = 1...2

    let directory: URL
    @Published private(set) var effectiveSpecs: [CommandSpec] = CommandRegistry.all
    /// The sequence lookup the key router reads on every key press, rebuilt by
    /// `reload()` so Reload Key Bindings reaches bare keys without a relaunch.
    @Published private(set) var table: KeySequenceTable = KeySequenceTable(specs: CommandRegistry.all)
    /// Non-fatal problems from the last `reload()`, named by kind and command
    /// (an unknown command id, an unparseable key, a duplicate, a sequence
    /// hidden behind a shorter one, a file from a newer version...). Empty = the
    /// file (if any) applied cleanly.
    @Published private(set) var problems: [KeyBindingProblem] = []
    /// `problems` as sentences, for the alert and older callers.
    var warnings: [String] { problems.map(\.message) }

    var fileURL: URL { directory.appendingPathComponent(Self.fileName) }

    init(directory: URL) {
        self.directory = directory
        reload()
    }

    /// Re-reads the file from disk and recomputes `effectiveSpecs` and `table`.
    /// Called once at launch (`AppModel.init`) and again only when the user
    /// clicks "Reload Key Bindings" in Settings — not on every file-system
    /// change (Assumption 9). The new table has a new `id`, so a router with a
    /// half-typed prefix drops it on its next key instead of completing it
    /// against the new bindings.
    func reload() {
        guard let data = try? Data(contentsOf: fileURL) else {
            install(CommandRegistry.all, problems: [])
            return
        }
        guard let file = try? JSONDecoder().decode(KeyBindingOverridesFile.self, from: data) else {
            install(CommandRegistry.all, problems: [KeyBindingProblem(
                commandId: nil, kind: .malformedFile,
                message: String(localized: "\(Self.fileName) is not valid JSON — using the built-in key bindings."))])
            return
        }
        guard Self.supportedVersions.contains(file.version) else {
            install(CommandRegistry.all, problems: [KeyBindingProblem(
                commandId: nil, kind: .unsupportedVersion,
                message: String(localized: "\(Self.fileName) has version \(file.version), but this version of Castellum reads versions \(Self.supportedVersions.lowerBound) to \(Self.supportedVersions.upperBound) — using the built-in key bindings."))])
            return
        }
        apply(file)
    }

    private func install(_ specs: [CommandSpec], problems: [KeyBindingProblem]) {
        effectiveSpecs = specs
        table = KeySequenceTable(specs: specs)
        self.problems = problems
    }

    /// Applies overrides one at a time, in `CommandID.allCases` order (a
    /// stable, deterministic order — independent of the JSON object's own
    /// key order, which `Dictionary` does not preserve). An override that
    /// would introduce a NEW problem against bindings already resolved so
    /// far is skipped (that one command keeps its built-in default); every
    /// other override still applies. This can never make a well-formed
    /// registry go from zero problems to nonzero — `CommandRegistry.problems(in:)`
    /// is checked after every single override, not just once at the end.
    private func apply(_ file: KeyBindingOverridesFile) {
        var specs = CommandRegistry.all
        var collected: [KeyBindingProblem] = []
        let file_ = Self.fileName
        for id in CommandID.allCases {
            guard var override = file.overrides[id.rawValue] else { continue }
            guard let index = specs.firstIndex(where: { $0.id == id }) else { continue }
            let name = id.rawValue
            // FP48: say so when a field cannot be parsed (the default is kept;
            // the other, valid fields of the same override still apply).
            if let raw = override.menuChord, MenuChord(display: raw) == nil {
                collected.append(KeyBindingProblem(
                    commandId: name, kind: .unparseable,
                    message: String(localized: "\(file_): \(name) menuChord '\(raw)' not understood — use the form shown in currentBindings (e.g. ⇧⌘R); default kept.")))
            }
            if let raw = override.bareKey, raw.count != 1 {
                collected.append(KeyBindingProblem(
                    commandId: name, kind: .unparseable,
                    message: String(localized: "\(file_): \(name) bareKey '\(raw)' must be exactly one character; default kept.")))
            }
            if let raws = override.keys {
                if let bad = raws.first(where: { KeySequence(display: $0) == nil }) {
                    // One bad sequence skips the whole `keys` entry: the command keeps its defaults.
                    collected.append(KeyBindingProblem(
                        commandId: name, kind: .unparseable,
                        message: String(localized: "\(file_): \(name) keys '\(bad)' not understood — use 1 to 4 keys separated by spaces, e.g. \"g i\" or \"⌃x a\" (no ⌘, Esc or Tab); default kept.")))
                    override.keys = nil
                } else if override.bareKey != nil {
                    collected.append(KeyBindingProblem(
                        commandId: name, kind: .ignored,
                        message: String(localized: "\(file_): \(name) has both keys and bareKey; bareKey is ignored.")))
                    override.bareKey = nil
                }
            }
            var trial = specs
            trial[index] = specs[index].applyingOverride(override)
            let conflicts = CommandRegistry.problems(in: trial)
            if conflicts.isEmpty {
                specs = trial
            } else {
                for conflict in conflicts {
                    collected.append(KeyBindingProblem(
                        commandId: name, kind: conflict.kind,
                        message: String(localized: "\(file_): \(name) override skipped — \(conflict.message)")))
                }
            }
        }
        let known = Set(CommandID.allCases.map(\.rawValue))
        for unknown in Set(file.overrides.keys).subtracting(known).sorted() {
            collected.append(KeyBindingProblem(
                commandId: unknown, kind: .unknownCommand,
                message: String(localized: "\(file_): unknown command id '\(unknown)' ignored.")))
        }
        install(specs, problems: collected)
    }

    /// Reveals the file in Finder, writing a documented starting point first
    /// if none exists yet: every command's CURRENT effective binding (so it
    /// can never drift from reality — generated from the live registry, the
    /// same discipline `ShortcutsView`'s derivation already established) plus
    /// an empty, ready-to-edit `overrides` object. Never overwrites a file
    /// the user already created.
    func revealFileInFinder() {
        writeDocumentedTemplateIfMissing()
        NSWorkspace.shared.activateFileViewerSelecting([fileURL])
    }

    /// The file-writing half of `revealFileInFinder()`, without the Finder
    /// activation — the only thing XCTest (`KeyBindingsStoreTests`) needs to
    /// exercise; production always calls `revealFileInFinder()`.
    func revealFileInFinderForTesting() { writeDocumentedTemplateIfMissing() }

    private func writeDocumentedTemplateIfMissing() {
        guard !FileManager.default.fileExists(atPath: fileURL.path) else { return }
        writeDocumentedTemplate()
    }

    private func writeDocumentedTemplate() {
        struct Entry: Codable {
            let title: String; let menuChord: String; let bareKey: String
            /// Every key sequence in file syntax ("e", "g i"); empty for a fixed or unbound command.
            let keys: [String]
        }
        struct Template: Codable {
            let version: Int
            let note: String
            let currentBindings: [String: Entry]
            let overrides: [String: KeyBindingOverride]
        }
        let currentBindings = Dictionary(uniqueKeysWithValues: effectiveSpecs.map { spec in
            (spec.id.rawValue, Entry(
                title: spec.title,
                menuChord: spec.menuChord?.display ?? "",
                bareKey: spec.bareKey.map(String.init) ?? "",
                keys: spec.sequences.map(\.fileSyntax)))
        })
        let template = Template(
            version: 2,
            note: String(
                localized: "currentBindings is a read-only snapshot taken when this file was created and is not updated afterwards (delete the file and use Reveal Key Bindings File… to regenerate it) — edit \"overrides\" instead. \"menuChord\" sets a menu shortcut (e.g. \"⇧⌘R\"); \"keys\" lists key sequences of 1 to 4 keys separated by spaces (e.g. [\"y\", \"g i\", \"⌃x a\"]) and replaces the command's current keys, [] unbinds it; upper case means Shift, and Return, Space, Delete and the arrows are written by name. ⌘, Esc and Tab cannot be used in a sequence. sendComposer (⌘⏎ in the compose window) and the Send Later presets have no rebindable keys. An override that collides with another command, or fails to parse, is skipped — Castellum never refuses to launch over a bad entry in this file."
            ),
            currentBindings: currentBindings,
            overrides: [:])
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(template) else { return }
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try? data.write(to: fileURL, options: .atomic)
    }
}
