// Castellum - S67: hardening tests that close S63's Diagnostics GUI gate gaps.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// S63's original click script is already covered by
// DiagnosticsViewModelTests.swift (folder/file-tree shape, redaction applied
// to app-log/crash-reports not the bundle, state transitions). This file
// closes the three specific gaps the human GUI gate cannot prove on its own:
//
//   1. The REAL production redaction path (Rust `redact_diagnostic_text` via
//      `CastellumCore.redactDiagnosticText`, exactly as `AppModel`'s
//      `DiagnosticsViewModel(model:)` convenience init wires it) strips a
//      realistic email address out of app-log/crash-report content — not a
//      synthetic "SENTINEL" marker.
//   2. `environment.txt`'s written content carries recognisable app version,
//      macOS version, schema version and generation-timestamp fields (not
//      just that the file exists, which the other test file already proves).
//   3. A structural, grep-based proof that no network client exists on the
//      diagnostics gather-and-save path.
//
// Deviation from the plan (project-docs/plans/2026-09-23-s63-xctest-hardening.md,
// Task 1): the plan asks for a realistic "account-identifier-shaped string"
// alongside the email address, modelled on the fixture convention used
// elsewhere in this test target ("acct-1"). Investigation of the real
// production wiring showed that is not what a real Castellum account
// identifier looks like: `AccountSetupSheet.derivedAccountId(email:host:existingAccountId:)`
// (apple/Sources/Castellum/App/AccountSetupSheet.swift:262-264) derives the
// stored `account_id` as the account's own email address (falling back to the
// host only when email is empty, an IMAP-username-only edge case). So for the
// realistic/default case, "the account identifier" and "the email address"
// are the SAME string, and proving email redaction on the real path also
// proves account-identifier redaction for that case. `redact_diagnostic_text`
// (native/crates/castellum-core/src/redact.rs) has no separate pattern for a
// bare non-email identifier (e.g. a host-derived id): that is a real, narrow
// residual gap for the empty-email edge case, not a test gap, and is out of
// this stage's File Structure (no `redact.rs` change listed) — flagged in the
// worker's final report rather than silently patched here.

import Foundation
import XCTest
@testable import Castellum

@MainActor
final class DiagnosticsHardeningTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-diagnostics-hardening-\(UUID().uuidString)")
    }

    // MARK: - Task 1: realistic-content redaction proof, through the REAL path

    /// Wires `redactText` to the REAL FFI call (`CastellumCore.redactDiagnosticText`,
    /// backed by Rust's `redact_diagnostic_text`) — the exact closure
    /// `DiagnosticsViewModel(model:)` uses in production — rather than the
    /// other test file's synthetic `{ $0.replacingOccurrences(of: "SENTINEL", ...) }`
    /// stand-in.
    @MainActor
    private func makeRealRedactionVM(
        appLog: @escaping () async -> String,
        crashReports: @escaping () -> [CrashReportFile],
        logDirectory: @escaping () -> URL? = { nil }
    ) throws -> DiagnosticsViewModel {
        let core = try CastellumCore(profilePath: tempProfile())
        return DiagnosticsViewModel(
            fetchLocalDiagnostics: { LocalDiagnostics(accounts: []) },
            fetchIntegrityCheck: { IntegrityReport(ok: true, issueCount: 0, firstIssue: nil) },
            fetchSupportBundle: {
                SupportBundle(
                    bundleSchema: 1,
                    generatedAt: "2026-09-23 10:00:00",
                    schemaVersion: 16,
                    coreVersion: "0.1.0",
                    integrity: IntegrityReport(ok: true, issueCount: 0, firstIssue: nil),
                    accounts: []
                )
            },
            fetchCrashReports: crashReports,
            fetchAppLog: appLog,
            fetchLogDirectory: logDirectory,
            redactText: { text in try core.redactDiagnosticText(text) }
        )
    }

    func testRealRedactionRemovesEmailFromAppLogAndCrashReports() async throws {
        // "someone@example.com" doubles as a realistic email AND — per
        // `derivedAccountId` — the realistic shape of a real account
        // identifier, since the two are the same string in production.
        let rawLog = """
        2026-09-23 09:58:01 Sync started for someone@example.com
        2026-09-23 09:58:03 NO [AUTHENTICATIONFAILED] for someone@example.com
        """
        let rawCrash = """
        Exception Type: EXC_BAD_ACCESS
        Thread 0 Crashed:
        Last known state: composing reply to someone@example.com about invoice #4821
        """
        let vm = try makeRealRedactionVM(
            appLog: { rawLog },
            crashReports: { [CrashReportFile(filename: "Castellum-crash.ips", contents: rawCrash)] }
        )
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave; got \(vm.exportState)")
            return
        }
        XCTAssertFalse(
            gathered.appLog.contains("someone@example.com"),
            "real redaction path left the email/account-identifier in app-log: \(gathered.appLog)"
        )
        XCTAssertTrue(gathered.appLog.contains("[redacted-address]"), "no redaction marker: \(gathered.appLog)")

        let crashContents = gathered.crashReports.first?.contents ?? ""
        XCTAssertFalse(
            crashContents.contains("someone@example.com"),
            "real redaction path left the email/account-identifier in the crash report: \(crashContents)"
        )
        XCTAssertTrue(crashContents.contains("[redacted-address]"), "no redaction marker: \(crashContents)")
        // Unrelated crash content on other lines must survive redaction.
        XCTAssertTrue(crashContents.contains("EXC_BAD_ACCESS"))
        XCTAssertTrue(crashContents.contains("invoice #4821"))
    }

    func testRealRedactionRemovesSecretLikeTokenFromCrashReport() async throws {
        // Defence-in-depth check alongside the email/account-id proof above:
        // the same real path also strips key=value credential-shaped text.
        let rawCrash = "connect retry failed: password=hunter2-not-a-real-secret giving up"
        let vm = try makeRealRedactionVM(
            appLog: { "ordinary log line, nothing sensitive" },
            crashReports: { [CrashReportFile(filename: "Castellum-crash2.ips", contents: rawCrash)] }
        )
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave; got \(vm.exportState)")
            return
        }
        let crashContents = gathered.crashReports.first?.contents ?? ""
        XCTAssertFalse(crashContents.contains("hunter2-not-a-real-secret"), "secret leaked: \(crashContents)")
        XCTAssertTrue(crashContents.contains("[redacted]"), "no redaction marker: \(crashContents)")
    }

    /// Same proof as `testRealRedactionRemovesEmailFromAppLogAndCrashReports`,
    /// extended to the new Rust-core-log excerpt (Task from the
    /// castellum_last_sync_kpi/castellum_log_directory diagnostics slice):
    /// the REAL `redact_diagnostic_text` path must strip a realistic email
    /// out of Rust log content exactly as it does for the app log and crash
    /// reports, before it ever reaches the exported bundle.
    func testRealRedactionRemovesEmailFromRustLogExcerpt() async throws {
        let dir = TestScratch.root
            .appendingPathComponent("rustlog-hardening-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let rawLog = """
        2026-09-23T09:58:01Z INFO castellum_core::sync: sync failed for someone@example.com: AUTHENTICATIONFAILED
        """
        try Data(rawLog.utf8).write(to: dir.appendingPathComponent("castellum.log"))

        let vm = try makeRealRedactionVM(
            appLog: { "ordinary log line, nothing sensitive" },
            crashReports: { [] },
            logDirectory: { dir }
        )
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave; got \(vm.exportState)")
            return
        }
        XCTAssertFalse(
            gathered.rustLog.contains("someone@example.com"),
            "real redaction path left the email in the Rust log excerpt: \(gathered.rustLog)"
        )
        XCTAssertTrue(gathered.rustLog.contains("[redacted-address]"), "no redaction marker: \(gathered.rustLog)")
        XCTAssertTrue(gathered.rustLog.contains("AUTHENTICATIONFAILED"), "unrelated log content must survive redaction")
    }

    // MARK: - Task 2: environment.txt field coverage

    func testSavedEnvironmentTxtContainsRecognisableFields() async throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let vm = DiagnosticsViewModel(
            fetchLocalDiagnostics: { LocalDiagnostics(accounts: []) },
            fetchIntegrityCheck: { IntegrityReport(ok: true, issueCount: 0, firstIssue: nil) },
            fetchSupportBundle: {
                SupportBundle(
                    bundleSchema: 1,
                    generatedAt: "2026-09-23 10:00:00",
                    schemaVersion: 16,
                    coreVersion: "0.1.0",
                    integrity: IntegrityReport(ok: true, issueCount: 0, firstIssue: nil),
                    accounts: []
                )
            },
            fetchCrashReports: { [] },
            fetchAppLog: { "log line" },
            redactText: { text in try core.redactDiagnosticText(text) }
        )
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave; got \(vm.exportState)")
            return
        }

        let parent = TestScratch.root
            .appendingPathComponent("env-txt-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }

        await vm.saveGatheredBundle(gathered, toParentDirectory: parent)
        guard case .saved(let folder) = vm.exportState else {
            XCTFail("Expected .saved; got \(vm.exportState)")
            return
        }
        let content = try String(
            contentsOf: folder.appendingPathComponent("environment.txt"), encoding: .utf8
        )

        // App version line: "Castellum <version> (<build>)".
        XCTAssertTrue(content.contains("Castellum "), "missing app version line: \(content)")
        // macOS version line.
        XCTAssertTrue(content.contains("macOS "), "missing macOS version line: \(content)")
        // Schema version — matches the bundle's schemaVersion (16 here).
        XCTAssertTrue(content.contains("Schema version: 16"), "missing schema version: \(content)")
        // Core version — matches the bundle's coreVersion.
        XCTAssertTrue(content.contains("Core version: 0.1.0"), "missing core version: \(content)")
        // Generation timestamp: "Generated: <yyyyMMdd-HHmmss> UTC".
        XCTAssertTrue(
            content.range(of: #"Generated: \d{8}-\d{6} UTC"#, options: .regularExpression) != nil,
            "missing/malformed generation timestamp: \(content)"
        )
    }

    // MARK: - Task 3: no-network structural proof

    /// Not a runtime test — this repo has no "networking disabled" test
    /// harness (per CLAUDE.md/the plan, building one for this alone is out of
    /// scope) — but a structural guarantee: the diagnostics gather-and-save
    /// path is DB-read + local-filesystem-write in both languages, with no
    /// HTTP client anywhere on it. Verified by grep at hardening time
    /// (2026-09-23) across the files that make up the path:
    ///
    ///   grep -n "URLSession\|ureq\|http://\|https://" \
    ///     apple/Sources/Castellum/App/DiagnosticsView.swift \
    ///     apple/Sources/Castellum/App/DiagnosticsBundleGatherer.swift \
    ///     native/crates/castellum-core/src/diagnostics.rs
    ///
    /// — zero hits in either file. `DiagnosticsBundleGatherer.swift` was
    /// added to this list alongside the Rust-log-tail read
    /// (`recentRustLog`): it is a local file read exactly like
    /// `recentAppLog`/`recentCrashReports` next to it, but the same
    /// structural guarantee should hold for it too, not just the two files
    /// this test already covered. This test re-asserts that at build time so
    /// a future PR introducing a network call on this path breaks CI instead
    /// of silently regressing a privacy guarantee.
    func testDiagnosticsGatherPathHasNoNetworkClientReferences() throws {
        let networkTokens = ["URLSession", "ureq", "http://", "https://"]
        let filesOnThePath = [
            "Sources/Castellum/App/DiagnosticsView.swift",
            "Sources/Castellum/App/DiagnosticsBundleGatherer.swift",
            "../native/crates/castellum-core/src/diagnostics.rs",
        ]
        let thisFile = URL(fileURLWithPath: #filePath)
        // apple/Tests/CastellumCoreTests/DiagnosticsHardeningTests.swift -> apple/
        let appleRoot = thisFile
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()

        for relativePath in filesOnThePath {
            let url = appleRoot.appendingPathComponent(relativePath)
            let contents = try String(contentsOf: url, encoding: .utf8)
            for token in networkTokens {
                XCTAssertFalse(
                    contents.contains(token),
                    "\(relativePath) references \(token) — the diagnostics gather-and-save path must stay network-free"
                )
            }
        }
    }
}
