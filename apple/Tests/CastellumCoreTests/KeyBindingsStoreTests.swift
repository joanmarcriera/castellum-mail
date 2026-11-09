// Castellum - KeyBindingsStore: load/apply/never-crash, temp directories only.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class KeyBindingsStoreTests: XCTestCase {
    private var dirs: [URL] = []

    private func tempDir() -> URL {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-keybindings-test-\(UUID().uuidString)", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        dirs.append(dir)
        return dir
    }

    override func tearDown() {
        for dir in dirs { try? FileManager.default.removeItem(at: dir) }
        dirs = []
    }

    private func store(_ json: String) throws -> KeyBindingsStore {
        let dir = tempDir()
        try json.write(to: dir.appendingPathComponent(KeyBindingsStore.fileName),
                       atomically: true, encoding: .utf8)
        return KeyBindingsStore(directory: dir)
    }

    /// FP48: an override that cannot be parsed is reported, not dropped silently.
    func testUnparseableChordProducesWarning() throws {
        let s = try store(#"{"version":1,"overrides":{"showStatus":{"menuChord":"⇧⌘9"},"showDiagnostics":{"menuChord":"cmd+shift+8"},"showOutbox":{"bareKey":"oo"}}}"#)
        XCTAssertTrue(s.warnings.contains { $0.contains("showDiagnostics") }, "\(s.warnings)")
        XCTAssertTrue(s.warnings.contains { $0.contains("showOutbox") }, "\(s.warnings)")
        XCTAssertFalse(s.warnings.contains { $0.contains("showStatus") }, "\(s.warnings)")
        XCTAssertEqual(s.effectiveSpecs.first { $0.id == .showStatus }?.menuChord?.display, "⇧⌘9")
    }

    /// FP48: an override onto a system chord (⌘,) would be dead: skip it.
    func testOverrideOntoReservedChordIsSkipped() throws {
        let s = try store(#"{"version":1,"overrides":{"showStatus":{"menuChord":"⌘,"}}}"#)
        XCTAssertEqual(s.effectiveSpecs.first { $0.id == .showStatus }?.menuChord,
                       CommandRegistry.spec(for: .showStatus).menuChord)
        XCTAssertEqual(s.warnings.count, 1, "\(s.warnings)")
        XCTAssertTrue(s.warnings.first?.contains("showStatus") ?? false)
    }

    /// Audit #3454: a command bound to the chord prefix 'g' could never fire.
    func testOverrideOntoTheChordPrefixIsSkippedWithAWarning() throws {
        let s = try store(#"{"version":1,"overrides":{"archive":{"bareKey":"g"}}}"#)
        XCTAssertEqual(s.effectiveSpecs.first { $0.id == .archive }?.bareKey,
                       CommandRegistry.spec(for: .archive).bareKey, "default key kept")
        XCTAssertTrue(s.warnings.contains { $0.contains("archive") && $0.contains("prefix") }, "\(s.warnings)")
        XCTAssertTrue(s.problems.contains { $0.commandId == "archive" && $0.kind == .shadowedByPrefix }, "\(s.problems)")
    }

    /// FP48: the template is written once; its note must not claim otherwise.
    func testTemplateNoteMatchesBehaviour() throws {
        let dir = tempDir()
        let s = KeyBindingsStore(directory: dir)
        s.revealFileInFinderForTesting()
        let text = try String(contentsOf: dir.appendingPathComponent(KeyBindingsStore.fileName), encoding: .utf8)
        XCTAssertFalse(text.contains("regenerated on every save"))
    }

    func testMissingFileUsesDefaultsWithNoWarnings() {
        let store = KeyBindingsStore(directory: tempDir())
        XCTAssertEqual(store.effectiveSpecs.count, CommandRegistry.all.count)
        XCTAssertEqual(store.warnings, [])
    }

    func testMalformedJsonFallsBackToDefaultsWithAWarning() throws {
        let dir = tempDir()
        try "{ not json".write(to: dir.appendingPathComponent(KeyBindingsStore.fileName),
                              atomically: true, encoding: .utf8)
        let store = KeyBindingsStore(directory: dir)
        XCTAssertEqual(store.effectiveSpecs.map(\.id), CommandRegistry.all.map(\.id))
        XCTAssertEqual(store.warnings.count, 1)
        XCTAssertTrue(store.warnings[0].contains("not valid JSON"))
    }

    func testValidOverrideRebindsTheBareKey() throws {
        let dir = tempDir()
        let json = #"{"version": 1, "overrides": {"toggleFlag": {"bareKey": "y"}}}"#
        try json.write(to: dir.appendingPathComponent(KeyBindingsStore.fileName),
                       atomically: true, encoding: .utf8)
        let store = KeyBindingsStore(directory: dir)
        let toggleFlag = store.effectiveSpecs.first { $0.id == .toggleFlag }!
        XCTAssertEqual(toggleFlag.bareKey, "y")
        XCTAssertEqual(store.warnings, [])
    }

    func testOverrideThatWouldCollideIsRejectedAndTheOriginalBindingSurvives() throws {
        let dir = tempDir()
        // "e" is already Archive's bare key — retargeting Compose onto it must be rejected.
        let json = #"{"version": 1, "overrides": {"compose": {"bareKey": "e"}}}"#
        try json.write(to: dir.appendingPathComponent(KeyBindingsStore.fileName),
                       atomically: true, encoding: .utf8)
        let store = KeyBindingsStore(directory: dir)
        let compose = store.effectiveSpecs.first { $0.id == .compose }!
        XCTAssertEqual(compose.bareKey, "c", "the collision must be rejected, not silently applied")
        XCTAssertEqual(CommandRegistry.conflicts(in: store.effectiveSpecs), [])
        XCTAssertEqual(store.warnings.count, 1)
        XCTAssertTrue(store.warnings[0].contains("compose"))
    }

    func testUnknownCommandIdIsIgnoredWithAWarning() throws {
        let dir = tempDir()
        let json = #"{"version": 1, "overrides": {"notACommand": {"bareKey": "y"}}}"#
        try json.write(to: dir.appendingPathComponent(KeyBindingsStore.fileName),
                       atomically: true, encoding: .utf8)
        let store = KeyBindingsStore(directory: dir)
        XCTAssertEqual(store.effectiveSpecs.map(\.id), CommandRegistry.all.map(\.id))
        XCTAssertTrue(store.warnings.contains { $0.contains("notACommand") })
    }

    func testRevealWritesADocumentedTemplateOnlyWhenNoFileExistsYet() {
        let dir = tempDir()
        let store = KeyBindingsStore(directory: dir)
        let fileURL = dir.appendingPathComponent(KeyBindingsStore.fileName)
        XCTAssertFalse(FileManager.default.fileExists(atPath: fileURL.path))
        // revealFileInFinder() also activates Finder, which is undesirable
        // under XCTest; exercise the pure write path it calls instead.
        store.revealFileInFinderForTesting()
        XCTAssertTrue(FileManager.default.fileExists(atPath: fileURL.path))
        let data = try! Data(contentsOf: fileURL)
        let json = try! JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertNotNil((json["currentBindings"] as? [String: Any])?["archive"])
        XCTAssertEqual((json["overrides"] as? [String: Any])?.count, 0)

        // Editing the file, then a second reveal must NOT clobber it.
        try! "{}".write(to: fileURL, atomically: true, encoding: .utf8)
        store.revealFileInFinderForTesting()
        XCTAssertEqual(try! String(contentsOf: fileURL, encoding: .utf8), "{}")
    }

    // MARK: MM-12 slice 1: version 2, sequences, named problems

    private func effective(_ s: KeyBindingsStore, _ id: CommandID) -> CommandSpec {
        s.effectiveSpecs.first { $0.id == id }!
    }

    /// A version 1 file gives exactly the effective specs it gave before this slice (golden).
    func testVersionOneFileKeepsItsPreSliceEffectiveSpecs() throws {
        let s = try store(#"{"version":1,"overrides":{"toggleFlag":{"bareKey":"y"},"showStatus":{"menuChord":"⇧⌘9"}}}"#)
        XCTAssertEqual(s.problems, [])
        XCTAssertEqual(effective(s, .toggleFlag).bareKey, "y")
        XCTAssertEqual(effective(s, .toggleFlag).sequences.map(\.fileSyntax), ["y"])
        XCTAssertEqual(effective(s, .showStatus).menuChord?.display, "⇧⌘9")
        // Everything else is the built-in table, binding for binding.
        for spec in CommandRegistry.all where spec.id != .toggleFlag && spec.id != .showStatus {
            let got = effective(s, spec.id)
            XCTAssertEqual(got.bareKey, spec.bareKey, "\(spec.id)")
            XCTAssertEqual(got.gChord, spec.gChord, "\(spec.id)")
            XCTAssertEqual(got.menuChord, spec.menuChord, "\(spec.id)")
            XCTAssertEqual(got.sequences, spec.sequences, "\(spec.id)")
        }
    }

    func testVersionTwoKeysReplaceTheBareKeyAndTheGChord() throws {
        let s = try store(#"{"version":2,"overrides":{"archive":{"keys":["y","⌃x a"]},"gotoInbox":{"keys":["g g"]}}}"#)
        XCTAssertEqual(s.problems, [])
        XCTAssertEqual(effective(s, .archive).sequences.map(\.fileSyntax), ["y", "⌃x a"])
        XCTAssertEqual(effective(s, .gotoInbox).sequences.map(\.fileSyntax), ["g g"])
        XCTAssertEqual(s.table.match(KeySequence(display: "y")!.strokes), .command(.archive))
        XCTAssertEqual(s.table.match(KeySequence(display: "e")!.strokes), .none, "the old bare key is replaced")
        XCTAssertEqual(s.table.match(KeySequence(display: "g i")!.strokes), .none)
        XCTAssertEqual(s.table.match(KeySequence(display: "g g")!.strokes), .command(.gotoInbox))
    }

    func testEmptyKeysUnbindsTheCommand() throws {
        let s = try store(#"{"version":2,"overrides":{"snooze":{"keys":[]}}}"#)
        XCTAssertEqual(s.problems, [])
        XCTAssertEqual(effective(s, .snooze).sequences, [])
        XCTAssertEqual(s.table.match(KeySequence(display: "b")!.strokes), .none)
    }

    func testKeysBesideBareKeyIgnoresBareKeyWithAProblem() throws {
        let s = try store(#"{"version":2,"overrides":{"archive":{"keys":["y"],"bareKey":"w"}}}"#)
        XCTAssertEqual(effective(s, .archive).sequences.map(\.fileSyntax), ["y"])
        XCTAssertEqual(s.problems.map(\.kind), [.ignored])
        XCTAssertEqual(s.problems.first?.commandId, "archive")
    }

    func testFutureVersionGivesDefaultsPlusOneUnsupportedVersionProblem() throws {
        let s = try store(#"{"version":3,"overrides":{"archive":{"keys":["y"]}}}"#)
        XCTAssertEqual(s.effectiveSpecs.map(\.id), CommandRegistry.all.map(\.id))
        XCTAssertEqual(effective(s, .archive).sequences.map(\.fileSyntax), ["e"], "defaults, nothing applied")
        XCTAssertEqual(s.problems.count, 1)
        XCTAssertEqual(s.problems.first?.kind, .unsupportedVersion)
        XCTAssertNil(s.problems.first?.commandId)
        XCTAssertTrue(s.problems[0].message.contains("3"), s.problems[0].message)
        let zero = try store(#"{"version":0,"overrides":{}}"#)
        XCTAssertEqual(zero.problems.map(\.kind), [.unsupportedVersion])
    }

    func testMalformedFileIsANamedProblem() throws {
        let s = try store("{ nope")
        XCTAssertEqual(s.problems.map(\.kind), [.malformedFile])
    }

    func testUnknownCommandIsANamedProblem() throws {
        let s = try store(#"{"version":2,"overrides":{"notACommand":{"keys":["y"]}}}"#)
        XCTAssertEqual(s.problems.map(\.kind), [.unknownCommand])
        XCTAssertEqual(s.problems.first?.commandId, "notACommand")
    }

    func testOneBadSequenceSkipsThatCommandOnly() throws {
        let s = try store(#"{"version":2,"overrides":{"archive":{"keys":["y","⌘k"]},"delete":{"keys":["d"]}}}"#)
        XCTAssertEqual(effective(s, .archive).sequences.map(\.fileSyntax), ["e"], "archive keeps its defaults")
        XCTAssertEqual(effective(s, .delete).sequences.map(\.fileSyntax), ["d"], "the other command still applies")
        XCTAssertEqual(s.problems.map(\.kind), [.unparseable])
        XCTAssertTrue(s.problems[0].message.contains("⌘k"), s.problems[0].message)
    }

    func testDuplicateAcrossTwoOverridesKeepsTheFirstInAllCasesOrder() throws {
        // archive precedes reply in CommandID.allCases? Whichever comes first wins; the other is a named duplicate.
        let ids = CommandID.allCases
        let first: CommandID = ids.firstIndex(of: .reply)! < ids.firstIndex(of: .archive)! ? .reply : .archive
        let second: CommandID = first == .reply ? .archive : .reply
        let s = try store(#"{"version":2,"overrides":{"archive":{"keys":["y"]},"reply":{"keys":["y"]}}}"#)
        XCTAssertEqual(effective(s, first).sequences.map(\.fileSyntax), ["y"])
        XCTAssertEqual(effective(s, second).sequences, CommandRegistry.spec(for: second).sequences)
        XCTAssertEqual(s.problems.filter { $0.kind == .duplicate }.count, 1, "\(s.problems)")
        XCTAssertEqual(s.problems.first { $0.kind == .duplicate }?.commandId, second.rawValue)
    }

    func testSequencesHiddenBehindAShorterOneAreRefused() throws {
        let s = try store(#"{"version":2,"overrides":{"archive":{"keys":["g"]}}}"#)
        XCTAssertEqual(effective(s, .archive).sequences.map(\.fileSyntax), ["e"])
        XCTAssertTrue(s.problems.contains { $0.kind == .shadowedByPrefix && $0.commandId == "archive" }, "\(s.problems)")
    }

    func testKeysOnAFixedCommandAreANotBindableProblem() throws {
        let s = try store(#"{"version":2,"overrides":{"sendComposer":{"keys":["y"]},"sendLaterMonday":{"keys":["m"]},"sendLater":{"bareKey":"x"},"insertSnippet":{"keys":["q"]}}}"#)
        XCTAssertEqual(s.problems.filter { $0.kind == .notBindable }.compactMap(\.commandId).sorted(),
                       ["insertSnippet", "sendComposer", "sendLater", "sendLaterMonday"])
        XCTAssertEqual(s.table.match(KeySequence(display: "q")!.strokes), .none,
                       "a compose-window command never gets a main-window key")
        XCTAssertEqual(effective(s, .sendComposer).sequences, [])
        XCTAssertEqual(s.table.match(KeySequence(display: "y")!.strokes), .none)
    }

    func testMenuChordOnACommandWithNoMenuItemIsANamedProblem() throws {
        let s = try store(#"{"version":2,"overrides":{"gotoInbox":{"menuChord":"⇧⌘9"},"nextMessage":{"menuChord":"⌃⌘J"}}}"#)
        XCTAssertEqual(s.problems.filter { $0.kind == .noMenuItem }.compactMap(\.commandId).sorted(), ["gotoInbox", "nextMessage"])
        XCTAssertNil(effective(s, .gotoInbox).menuChord)
        XCTAssertTrue(s.problems[0].message.contains("keys"), "says to use keys: \(s.problems[0].message)")
    }

    func testReservedChordIsANamedProblem() throws {
        let s = try store(#"{"version":2,"overrides":{"showStatus":{"menuChord":"⌘,"}}}"#)
        XCTAssertTrue(s.problems.contains { $0.kind == .reservedChord && $0.commandId == "showStatus" }, "\(s.problems)")
    }

    func testWarningsAreTheProblemMessages() throws {
        let s = try store(#"{"version":2,"overrides":{"archive":{"keys":["⌘k"]},"zzz":{"keys":["y"]}}}"#)
        XCTAssertFalse(s.problems.isEmpty)
        XCTAssertEqual(s.warnings, s.problems.map(\.message))
    }

    func testReloadRebuildsTheTableAndChangesItsId() throws {
        let dir = tempDir()
        let s = KeyBindingsStore(directory: dir)
        let before = s.table.id
        try #"{"version":2,"overrides":{"archive":{"keys":["y"]}}}"#
            .write(to: dir.appendingPathComponent(KeyBindingsStore.fileName), atomically: true, encoding: .utf8)
        s.reload()
        XCTAssertNotEqual(s.table.id, before)
        XCTAssertEqual(s.table.match(KeySequence(display: "y")!.strokes), .command(.archive))
        // And back: delete the file, reload, defaults again.
        try FileManager.default.removeItem(at: dir.appendingPathComponent(KeyBindingsStore.fileName))
        s.reload()
        XCTAssertEqual(s.table.match(KeySequence(display: "e")!.strokes), .command(.archive))
        XCTAssertEqual(s.problems, [])
    }

    func testTemplateIsVersionTwoWithKeysPerCommand() throws {
        let dir = tempDir()
        let s = KeyBindingsStore(directory: dir)
        s.revealFileInFinderForTesting()
        let data = try Data(contentsOf: dir.appendingPathComponent(KeyBindingsStore.fileName))
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(json["version"] as? Int, 2)
        let current = try XCTUnwrap(json["currentBindings"] as? [String: [String: Any]])
        XCTAssertEqual(current["archive"]?["keys"] as? [String], ["e"])
        XCTAssertEqual(current["gotoInbox"]?["keys"] as? [String], ["g i"])
        XCTAssertEqual(current["sendComposer"]?["keys"] as? [String], [])
        XCTAssertTrue((json["note"] as? String ?? "").contains("sendComposer"), "the note names the fixed command")
        // The template it wrote loads cleanly (an untouched snapshot has no overrides).
        XCTAssertEqual(KeyBindingsStore(directory: dir).problems, [])
    }
}
