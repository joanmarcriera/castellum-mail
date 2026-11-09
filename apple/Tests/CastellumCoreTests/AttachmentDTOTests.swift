// Castellum - attachment DTO decode + download-state tests (S32).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Every AppModel here runs against a temp profile directory (never the real
// profile or the real Keychain namespace); tearDown wipes the per-profile
// Keychain namespace exactly like the other AppModel-level tests
// (see UndoSendReopenTests, commits 9b6a56d9/00aa0bda).

import XCTest
@testable import Castellum

@MainActor
final class AttachmentDTOTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-attachments-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    func testAttachmentDownloadResultDecodesAllStatuses() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        for status in ["cached", "not_cached", "downloaded", "failed"] {
            let json = """
            {"status":"\(status)","local_path":null,"error":null}
            """
            let result = try decoder.decode(AttachmentDownloadResult.self, from: Data(json.utf8))
            XCTAssertEqual(result.status, status)
        }
    }

    func testAttachmentDownloadResultDecodesLocalPathAndError() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let json = """
        {"status":"failed","local_path":"/tmp/x","error":"mailbox UIDVALIDITY changed"}
        """
        let result = try decoder.decode(AttachmentDownloadResult.self, from: Data(json.utf8))
        XCTAssertEqual(result.localPath, "/tmp/x")
        XCTAssertEqual(result.error, "mailbox UIDVALIDITY changed")
    }

    func testAttachmentKeyIsStableForSameMessageAndOrdinal() {
        let model = makeModel()
        let key1 = model.attachmentKey(messageId: "m1", ordinal: 0)
        let key2 = model.attachmentKey(messageId: "m1", ordinal: 0)
        let key3 = model.attachmentKey(messageId: "m1", ordinal: 1)
        XCTAssertEqual(key1, key2)
        XCTAssertNotEqual(key1, key3)
    }

    /// No account was ever stored, so the Keychain-backed config lookup fails
    /// and the state machine must land on `.failed` synchronously — never
    /// `.downloading` with no task behind it.
    func testDownloadWithoutStoredCredentialsFailsImmediately() {
        let model = makeModel()
        let key = model.attachmentKey(messageId: "m1", ordinal: 0)
        model.downloadAttachment(messageId: "m1", ordinal: 0, accountId: "no-such-account")
        guard case .failed(let reason) = model.attachmentDownloads[key] else {
            return XCTFail("expected .failed, got \(String(describing: model.attachmentDownloads[key]))")
        }
        XCTAssertTrue(reason.contains("credentials"))
    }

    /// A message that was never downloaded reports `not_cached` from the
    /// local-only poll, which the model maps to `.idle` (the strip then shows
    /// its Download button).
    func testRefreshForUncachedAttachmentSettlesOnIdle() async throws {
        let model = makeModel()
        let key = model.attachmentKey(messageId: "acc:none", ordinal: 0)
        model.refreshAttachmentCacheStatus(messageId: "acc:none", ordinal: 0)
        XCTAssertEqual(model.attachmentDownloads[key], .checking)

        let deadline = Date().addingTimeInterval(5)
        while model.attachmentDownloads[key] == .checking && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertEqual(model.attachmentDownloads[key], .idle)
    }

    /// Cancelling a request that was never started must be a harmless no-op.
    func testCancelWithoutARequestLeavesStateUntouched() {
        let model = makeModel()
        let key = model.attachmentKey(messageId: "m1", ordinal: 0)
        model.cancelAttachmentDownload(messageId: "m1", ordinal: 0)
        XCTAssertNil(model.attachmentDownloads[key])
    }

    // MARK: - safe filenames

    func testSanitizedAttachmentFilenameStripsPathTraversalAndSlashes() {
        XCTAssertEqual(sanitizedAttachmentFilename("../../etc/passwd"), "passwd")
        XCTAssertEqual(sanitizedAttachmentFilename("a/b/c.pdf"), "c.pdf")
        XCTAssertEqual(sanitizedAttachmentFilename("normal.pdf"), "normal.pdf")
        XCTAssertEqual(sanitizedAttachmentFilename(""), "attachment")
        XCTAssertEqual(sanitizedAttachmentFilename(nil), "attachment")
        XCTAssertEqual(sanitizedAttachmentFilename("."), "attachment")
        XCTAssertEqual(sanitizedAttachmentFilename(".."), "attachment")
    }

    func testSanitizedAttachmentFilenameHandlesWindowsSeparatorsControlsAndHiddenNames() {
        XCTAssertEqual(sanitizedAttachmentFilename("C:\\Users\\x\\report.pdf"), "report.pdf")
        XCTAssertEqual(sanitizedAttachmentFilename("in\u{0000}voice.pdf"), "invoice.pdf")
        XCTAssertEqual(sanitizedAttachmentFilename(".hidden.pdf"), "hidden.pdf")
        XCTAssertEqual(sanitizedAttachmentFilename("..."), "attachment")
        XCTAssertEqual(sanitizedAttachmentFilename("   "), "attachment")
    }

    func testSanitizedAttachmentFilenameCapsLengthKeepingTheExtension() {
        let long = String(repeating: "a", count: 400) + ".pdf"
        let safe = sanitizedAttachmentFilename(long)
        XCTAssertLessThanOrEqual(safe.utf8.count, 200)
        XCTAssertTrue(safe.hasSuffix(".pdf"))
    }

    // MARK: - no auto-open of executables

    func testExecutableAttachmentIsBlockedFromAutoOpen() {
        XCTAssertTrue(isBlockedFromAutoOpen(filename: "installer.pkg"))
        XCTAssertTrue(isBlockedFromAutoOpen(filename: "run.command"))
        XCTAssertTrue(isBlockedFromAutoOpen(filename: "Script.scpt"))
        XCTAssertTrue(isBlockedFromAutoOpen(filename: "Disk.dmg"))
        XCTAssertTrue(isBlockedFromAutoOpen(filename: "Thing.APP"), "extension match is case-insensitive")
        XCTAssertFalse(isBlockedFromAutoOpen(filename: "report.pdf"))
        XCTAssertFalse(isBlockedFromAutoOpen(filename: "photo.png"))
        XCTAssertFalse(isBlockedFromAutoOpen(filename: "attachment"))
    }

    func testDeclaredExecutableContentTypeBlocksAnExtensionlessName() {
        XCTAssertTrue(isBlockedFromAutoOpen(filename: "attachment", contentType: "application/x-mach-binary"))
        XCTAssertFalse(isBlockedFromAutoOpen(filename: "attachment", contentType: "application/pdf"))
    }

    func testSanitizedAttachmentFilenameKeepsControlAndFormatStrippingPinned() {
        // CharacterSet.controlCharacters covers Cc AND Cf (#3379): pin it.
        XCTAssertEqual(sanitizedAttachmentFilename("a\u{202E}fdp.scr"), "afdp.scr")
        XCTAssertEqual(sanitizedAttachmentFilename("in\u{200B}voice\u{FEFF}.pdf"), "invoice.pdf")
    }

    func testSanitizedAttachmentFilenameCollapsesPaddingSoTheRealExtensionShows() {
        for pad in [" ", "\u{00A0}", "\u{3000}", "\u{2800}", "\u{3164}", "\u{2028}"] {
            let raw = "invoice.pdf" + String(repeating: pad, count: 150) + ".scr"
            XCTAssertEqual(sanitizedAttachmentFilename(raw), "invoice.pdf.scr", "padding \(pad.unicodeScalars.first!.value)")
        }
        XCTAssertEqual(sanitizedAttachmentFilename("my   report.pdf"), "my report.pdf")
    }

    func testMoreCodeRunningAndLocationTypesAreBlockedFromAutoOpen() {
        for name in [
            "Invoice.terminal", "x.inetloc", "x.fileloc", "x.webloc", "x.url", "a.scr", "a.osax",
            "a.saver", "a.prefPane", "a.kext", "a.plugin", "a.xip", "a.iso", "a.sparseimage",
            "a.jnlp", "a.mobileconfig", "a.shortcut", "a.wflow", "a.fish",
            // via UTType conformance rather than the extension list
            "a.zsh", "a.py", "a.rb", "a.pl", "a.php", "a.applescript",
        ] {
            XCTAssertTrue(isBlockedFromAutoOpen(filename: name), name)
        }
        for name in ["a.pdf", "a.png", "invite.ics", "card.vcf", "notes.txt", "a.docx", "a.zip"] {
            XCTAssertFalse(isBlockedFromAutoOpen(filename: name), name)
        }
    }

    // MARK: - quarantine + staging (Save As / drag-out / Open / Quick Look)

    private func makeTempDir() throws -> URL {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-attachment-files-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return dir
    }

    func testSaveQuarantinedCopiesBytesAndSetsTheQuarantineAttribute() throws {
        let dir = try makeTempDir()
        let source = dir.appendingPathComponent("cache-hash")
        try Data("hello world".utf8).write(to: source)
        let destination = dir.appendingPathComponent("saved.pdf")

        try saveQuarantined(from: source, to: destination)

        XCTAssertEqual(try Data(contentsOf: destination), Data("hello world".utf8))
        let values = try destination.resourceValues(forKeys: [.quarantinePropertiesKey])
        XCTAssertNotNil(values.quarantineProperties, "saved attachment must carry com.apple.quarantine")
        // The cache file itself is never quarantined or modified.
        XCTAssertNil(try source.resourceValues(forKeys: [.quarantinePropertiesKey]).quarantineProperties)
    }

    func testSaveQuarantinedOverwritesAnExistingDestination() throws {
        let dir = try makeTempDir()
        let source = dir.appendingPathComponent("cache-hash")
        try Data("new".utf8).write(to: source)
        let destination = dir.appendingPathComponent("saved.txt")
        try Data("old".utf8).write(to: destination)

        try saveQuarantined(from: source, to: destination)
        XCTAssertEqual(try Data(contentsOf: destination), Data("new".utf8))
    }

    func testStageAttachmentCopyUsesTheSanitisedFilenameNotTheCacheHash() throws {
        let dir = try makeTempDir()
        let cache = dir.appendingPathComponent("cache", isDirectory: true)
        try FileManager.default.createDirectory(at: cache, withIntermediateDirectories: true)
        let cached = cache.appendingPathComponent("deadbeefcafe")
        try Data("pdf-bytes".utf8).write(to: cached)
        let root = dir.appendingPathComponent("staging", isDirectory: true)

        let staged = try stageAttachmentCopy(
            cachePath: cached.path, filename: "../../evil/report.pdf", stagingRoot: root)

        XCTAssertEqual(staged.lastPathComponent, "report.pdf")
        XCTAssertTrue(staged.path.hasPrefix(root.path), "staged copy must stay inside the staging root")
        XCTAssertEqual(try Data(contentsOf: staged), Data("pdf-bytes".utf8))
        XCTAssertNotNil(
            try staged.resourceValues(forKeys: [.quarantinePropertiesKey]).quarantineProperties)
    }

    func testStageAttachmentCopyAlwaysProducesAFreshCopy() throws {
        let dir = try makeTempDir()
        let cached = dir.appendingPathComponent("cafebabe")
        try Data("original".utf8).write(to: cached)
        let root = dir.appendingPathComponent("staging", isDirectory: true)

        let first = try stageAttachmentCopy(cachePath: cached.path, filename: "a.txt", stagingRoot: root)
        try Data("edited by the user".utf8).write(to: first)
        let second = try stageAttachmentCopy(cachePath: cached.path, filename: "a.txt", stagingRoot: root)

        XCTAssertEqual(first, second)
        XCTAssertEqual(try Data(contentsOf: second), Data("original".utf8))
    }

    func testPruneStagedAttachmentsRemovesOnlyStaleDirectories() throws {
        let dir = try makeTempDir()
        let root = dir.appendingPathComponent("staging", isDirectory: true)
        let stale = root.appendingPathComponent("stale", isDirectory: true)
        let fresh = root.appendingPathComponent("fresh", isDirectory: true)
        try FileManager.default.createDirectory(at: stale, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: fresh, withIntermediateDirectories: true)

        // Backdate `stale` three days; `fresh` keeps its just-created mtime.
        try FileManager.default.setAttributes(
            [.modificationDate: Date().addingTimeInterval(-3 * 24 * 3600)], ofItemAtPath: stale.path)
        pruneStagedAttachments(root: root, olderThan: 24 * 3600, now: Date())

        XCTAssertFalse(FileManager.default.fileExists(atPath: stale.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: fresh.path))
    }

    // MARK: - Save All / Save as Zip (S76)

    func testUniqueAttachmentFilenamesDeduplicatesWithNumericSuffix() {
        let names = uniqueAttachmentFilenames(["report.pdf", "report.pdf", "report.pdf", "notes", "notes"])
        XCTAssertEqual(names, ["report.pdf", "report 2.pdf", "report 3.pdf", "notes", "notes 2"])
    }

    func testUniqueAttachmentFilenamesIsCaseInsensitiveAndSanitises() {
        let names = uniqueAttachmentFilenames(["A.txt", "a.TXT", "../../evil", nil])
        XCTAssertEqual(names, ["A.txt", "a 2.TXT", "evil", "attachment"])
    }

    func testSaveAllQuarantinedWritesEveryFileInsideTheFolderWithQuarantine() throws {
        let dir = try makeTempDir()
        let s1 = dir.appendingPathComponent("h1"), s2 = dir.appendingPathComponent("h2")
        let s3 = dir.appendingPathComponent("h3")
        try Data("one".utf8).write(to: s1)
        try Data("two".utf8).write(to: s2)
        try Data("evil".utf8).write(to: s3)
        let folder = dir.appendingPathComponent("chosen", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)

        let written = try saveAllQuarantined(
            [(s1, "report.pdf"), (s2, "report.pdf"), (s3, "../../evil")], into: folder)

        XCTAssertEqual(written.map(\.lastPathComponent), ["report.pdf", "report 2.pdf", "evil"])
        for url in written {
            XCTAssertEqual(url.deletingLastPathComponent().standardizedFileURL.path, folder.standardizedFileURL.path)
            XCTAssertNotNil(
                try url.resourceValues(forKeys: [.quarantinePropertiesKey]).quarantineProperties,
                "\(url.lastPathComponent) must carry com.apple.quarantine")
        }
        XCTAssertEqual(try String(contentsOf: written[1]), "two")
        XCTAssertFalse(FileManager.default.fileExists(
            atPath: dir.deletingLastPathComponent().appendingPathComponent("evil").path))
    }

    func testSaveAllQuarantinedPreservesExistingFiles() throws {
        let dir = try makeTempDir()
        let src = dir.appendingPathComponent("h1")
        try Data("new".utf8).write(to: src)
        let folder = dir.appendingPathComponent("chosen", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let mine = folder.appendingPathComponent("invoice.pdf")
        try Data("mine".utf8).write(to: mine)

        let written = try saveAllQuarantined([(src, "invoice.pdf")], into: folder)

        XCTAssertEqual(try String(contentsOf: mine), "mine", "the user's own file must survive")
        XCTAssertEqual(written.map(\.lastPathComponent), ["invoice 2.pdf"])
        XCTAssertEqual(try String(contentsOf: written[0]), "new")
    }

    func testUniqueAttachmentFilenamesTreatsNFCAndNFDAsDuplicates() {
        let nfd = "cafe\u{301}.txt", nfc = "caf\u{e9}.txt"
        let names = uniqueAttachmentFilenames([nfd, nfc])
        XCTAssertEqual(names.count, 2)
        XCTAssertNotEqual(names[0].precomposedStringWithCanonicalMapping,
                          names[1].precomposedStringWithCanonicalMapping)
    }

    func testIdleDecisionStopsWhenUserCancelledAStartedDownload() {
        XCTAssertTrue(AttachmentBulkPlan.idleDecision(idle: [1], started: [1, 2]).userCancelled)
        let fresh = AttachmentBulkPlan.idleDecision(idle: [3], started: [1, 2])
        XCTAssertFalse(fresh.userCancelled)
        XCTAssertEqual(fresh.start, [3])
    }

    func testSaveZipQuarantinedProducesAQuarantinedArchiveOfDedupedSanitisedNames() throws {
        let dir = try makeTempDir()
        let s1 = dir.appendingPathComponent("h1"), s2 = dir.appendingPathComponent("h2")
        try Data("one".utf8).write(to: s1)
        try Data("two".utf8).write(to: s2)
        let zip = dir.appendingPathComponent("out.zip")

        try saveZipQuarantined([(s1, "a.txt"), (s2, "../a.txt")], to: zip)

        XCTAssertNotNil(try zip.resourceValues(forKeys: [.quarantinePropertiesKey]).quarantineProperties)
        let list = Process()
        list.executableURL = URL(fileURLWithPath: "/usr/bin/zipinfo")
        list.arguments = ["-1", zip.path]
        let pipe = Pipe()
        list.standardOutput = pipe
        try list.run()
        let out = String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        list.waitUntilExit()
        XCTAssertEqual(Set(out.split(separator: "\n").map(String.init)), ["a.txt", "a 2.txt"])
    }
}
