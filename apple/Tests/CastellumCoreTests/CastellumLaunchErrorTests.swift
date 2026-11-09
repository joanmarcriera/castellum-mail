// Castellum - launch-time profile-open refusal classification.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class CastellumLaunchErrorTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-launch-error-\(UUID().uuidString)")
    }

    // MARK: - pure mapping (each recognised prefix -> its case + message)

    func testSchemaTooNewPrefixMapsToItsCaseAndStripsTheMarker() {
        let reason = "\(CastellumLaunchError.schemaTooNewPrefix)This profile was created by a " +
            "newer version of Castellum (schema 99). Install the latest version of Castellum " +
            "to open it. This build supports up to schema 34."
        guard case .schemaTooNew(let message)? = CastellumLaunchError.classify(CastellumError.message(reason))
        else {
            return XCTFail("expected .schemaTooNew")
        }
        XCTAssertFalse(message.hasPrefix("schema too new"), "the machine marker must be stripped")
        XCTAssertTrue(message.contains("newer version of Castellum"))
        XCTAssertTrue(message.contains("schema 99"))
    }

    func testMigrationFailedPrefixMapsToItsCaseAndStripsTheMarker() {
        let reason = "\(CastellumLaunchError.migrationFailedPrefix)Castellum couldn't upgrade " +
            "your mail database. Your data is unchanged. A backup was saved at /tmp/x.bak."
        guard case .migrationFailed(let message)? = CastellumLaunchError.classify(
            CastellumError.message(reason))
        else {
            return XCTFail("expected .migrationFailed")
        }
        XCTAssertFalse(message.hasPrefix("migration failed"), "the machine marker must be stripped")
        XCTAssertTrue(message.contains("Your data is unchanged"))
        XCTAssertTrue(message.contains("/tmp/x.bak"))
    }

    /// Every other error stays unclassified — it must keep crashing via
    /// `AppModel`'s `fatalError`, not silently degrade into a launch alert.
    func testUnrelatedErrorsAreNotClassified() {
        XCTAssertNil(CastellumLaunchError.classify(CastellumError.io))
        XCTAssertNil(CastellumLaunchError.classify(CastellumError.message("some other reason entirely")))
        XCTAssertNil(CastellumLaunchError.classify(CastellumError.internalError))
    }

    /// Titles/messages are never empty — whatever alert `AppModel` presents
    /// always has real content for both cases.
    func testTitlesAndMessagesAreNonEmptyForBothCases() {
        for error in [
            CastellumLaunchError.schemaTooNew(message: "x"),
            CastellumLaunchError.migrationFailed(message: "y"),
        ] {
            XCTAssertFalse(error.title.isEmpty)
            XCTAssertFalse(error.message.isEmpty)
        }
    }

    // MARK: - end to end, against a real (temp-only) profile

    /// The actual launch path: open a fresh temp profile (fully migrates),
    /// stamp its `_migrations` watermark past what this build's `MIGRATIONS`
    /// knows (via the `sqlite3` CLI directly on the file — never through
    /// Castellum's own FFI, which has no "corrupt this profile" entry point
    /// by design), then reopen through the SAME `CastellumCore.init` the app
    /// calls at launch and confirm it throws something
    /// `CastellumLaunchError.classify` recognises as `.schemaTooNew`. Uses
    /// only a disposable temp directory — never the real profile, never the
    /// Keychain (this path never touches it).
    func testTheRealLaunchPathRefusesAProfileStampedNewerThanThisBuildKnows() throws {
        let dir = tempProfile()
        let dbPath = dir.appendingPathComponent("castellum.db").path

        let futureVersion: Int64
        do {
            let core = try CastellumCore(profilePath: dir)
            futureVersion = try core.schemaVersion() + 1
        } // `core` deinits here, closing the SQLite connection before the
        // external process below touches the same file.

        let sqlite3CLI = Process()
        sqlite3CLI.executableURL = URL(fileURLWithPath: "/usr/bin/sqlite3")
        sqlite3CLI.arguments = [
            dbPath,
            "INSERT INTO _migrations (id, name) VALUES (\(futureVersion), 'from_swift_test');",
        ]
        try sqlite3CLI.run()
        sqlite3CLI.waitUntilExit()
        XCTAssertEqual(sqlite3CLI.terminationStatus, 0, "stamping the test fixture must succeed")

        do {
            _ = try CastellumCore(profilePath: dir)
            XCTFail("must refuse a schema newer than this build supports")
        } catch {
            guard let launchError = CastellumLaunchError.classify(error) else {
                return XCTFail("expected a classified launch error, got \(error)")
            }
            guard case .schemaTooNew(let message) = launchError else {
                return XCTFail("expected .schemaTooNew, got \(launchError)")
            }
            XCTAssertTrue(message.contains("newer version of Castellum"))
            XCTAssertTrue(message.contains("\(futureVersion)"))
        }

        try? FileManager.default.removeItem(at: dir)
    }
}
