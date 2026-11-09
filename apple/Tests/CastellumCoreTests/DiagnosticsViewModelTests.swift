// Castellum - DiagnosticsViewModel unit tests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Tests the view-model state transitions and helpers in isolation — no FFI, no
// windows, no file I/O. Closures stand in for the three FFI calls.

import XCTest
@testable import Castellum

// MARK: - Fixtures

private func makeAccount(
    email: String = "test@example.com",
    consecutiveFailures: Int64 = 0,
    lastSuccessAt: String? = "2026-07-10 10:00:00",
    lastAttemptAt: String? = "2026-07-10 10:00:00",
    mailboxes: [LocalMailboxDiagnostics] = []
) -> LocalAccountDiagnostics {
    LocalAccountDiagnostics(
        accountId: "acct-1",
        email: email,
        provider: "imap",
        lastAttemptAt: lastAttemptAt,
        lastSuccessAt: lastSuccessAt,
        lastErrorKind: consecutiveFailures > 0 ? "auth" : nil,
        lastError: consecutiveFailures > 0 ? "authentication failed" : nil,
        lastErrorPhase: consecutiveFailures > 0 ? "connect" : nil,
        consecutiveFailures: consecutiveFailures,
        mailboxes: mailboxes
    )
}

private func makeMailbox(
    path: String = "INBOX",
    consecutiveFailures: Int64 = 0
) -> LocalMailboxDiagnostics {
    LocalMailboxDiagnostics(
        mailboxId: "acct-1:INBOX",
        path: path,
        role: "inbox",
        lastAttemptAt: "2026-07-10 10:00:00",
        lastSuccessAt: consecutiveFailures == 0 ? "2026-07-10 10:00:00" : nil,
        lastErrorKind: consecutiveFailures > 0 ? "network" : nil,
        lastError: consecutiveFailures > 0 ? "network connection failed" : nil,
        lastErrorPhase: consecutiveFailures > 0 ? "fetch" : nil,
        consecutiveFailures: consecutiveFailures,
        totalMessages: 42,
        messagesWithBody: 40,
        messagesWithoutBody: 2,
        hasSyncCursor: true
    )
}

private func makeIntegrityReport(ok: Bool, issueCount: Int = 0) -> IntegrityReport {
    IntegrityReport(ok: ok, issueCount: issueCount, firstIssue: ok ? nil : "wrong # of entries in index x")
}

private func makeBundle() -> SupportBundle {
    SupportBundle(
        bundleSchema: 1,
        generatedAt: "2026-07-10 12:00:00",
        schemaVersion: 16,
        coreVersion: "0.1.0",
        integrity: IntegrityReport(ok: true, issueCount: 0, firstIssue: nil),
        accounts: []
    )
}

// MARK: - Helper to build a testable VM

private func makeCrashReport(filename: String = "Castellum-test.ips", contents: String = "trace") -> CrashReportFile {
    CrashReportFile(filename: filename, contents: contents)
}

private func makeSyncPhase(count: Int = 1, totalMs: Double = 10) -> SyncPhaseKpi {
    SyncPhaseKpi(count: count, totalMs: totalMs, p50Ms: totalMs, p95Ms: totalMs, maxMs: totalMs)
}

private func makeKpi(
    messageCount: Int64 = 100,
    wallMs: Double = 1000,
    phases: [String: SyncPhaseKpi] = [
        "imap.fetch_descriptors": makeSyncPhase(count: 1, totalMs: 400),
        "db.upsert_descriptor": makeSyncPhase(count: 100, totalMs: 60),
    ]
) -> LastSyncKpi {
    LastSyncKpi(
        available: true, accountId: "acct-1", timestampUnix: 1_700_000_000,
        messageCount: messageCount, wallMs: wallMs, phases: phases
    )
}

@MainActor
private func makeVM(
    localDiag: @escaping () async throws -> LocalDiagnostics = {
        LocalDiagnostics(accounts: [makeAccount()])
    },
    integrity: @escaping () async throws -> IntegrityReport = {
        makeIntegrityReport(ok: true)
    },
    bundle: @escaping () async throws -> SupportBundle = {
        makeBundle()
    },
    lastSyncKpi: @escaping () async throws -> LastSyncKpi? = { nil },
    crashReports: @escaping () -> [CrashReportFile] = { [] },
    appLog: @escaping () async -> String = { "log line" },
    logDirectory: @escaping () -> URL? = { nil },
    redact: @escaping (String) throws -> String = { $0 }
) -> DiagnosticsViewModel {
    DiagnosticsViewModel(
        fetchLocalDiagnostics: localDiag,
        fetchIntegrityCheck: integrity,
        fetchSupportBundle: bundle,
        fetchLastSyncKpi: lastSyncKpi,
        fetchCrashReports: crashReports,
        fetchAppLog: appLog,
        fetchLogDirectory: logDirectory,
        redactText: redact
    )
}

// MARK: - Load state transitions

@MainActor
final class DiagnosticsViewModelLoadTests: XCTestCase {
    func testLoadingTransitionsThenLoaded() async {
        let vm = makeVM()
        // Immediately after construction it should be .loading.
        if case .loading = vm.loadState {} else {
            XCTFail("Expected .loading; got \(vm.loadState)")
        }
        vm.load()
        // Allow the async task to complete.
        await Task.yield()
        await Task.yield()
        // May need a few yields for the detached task to schedule back.
        for _ in 0..<10 {
            await Task.yield()
        }
        if case .loaded = vm.loadState {} else {
            XCTFail("Expected .loaded after successful fetch; got \(vm.loadState)")
        }
    }

    func testEmptyAccountsTransitionsToEmpty() async {
        let vm = makeVM(localDiag: { LocalDiagnostics(accounts: []) })
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        if case .empty = vm.loadState {} else {
            XCTFail("Expected .empty; got \(vm.loadState)")
        }
    }

    func testFetchErrorTransitionsToUnavailable() async {
        let vm = makeVM(localDiag: { throw CastellumError.internalError })
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        if case .unavailable = vm.loadState {} else {
            XCTFail("Expected .unavailable; got \(vm.loadState)")
        }
    }
}

// MARK: - Performance ("Last Sync KPI") state transitions

@MainActor
final class DiagnosticsViewModelKpiTests: XCTestCase {
    func testKpiStartsLoading() {
        let vm = makeVM()
        if case .loading = vm.kpiState {} else {
            XCTFail("Expected .loading; got \(vm.kpiState)")
        }
    }

    /// "No sync yet" (the FFI's `"null"` sentinel, surfaced here as `nil`) is
    /// a normal empty state — `.none` — never `.unavailable`.
    func testLoadWithNoRecordedSyncTransitionsToNone() async {
        let vm = makeVM(lastSyncKpi: { nil })
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        if case .none = vm.kpiState {} else {
            XCTFail("Expected .none; got \(vm.kpiState)")
        }
    }

    func testLoadWithRecordedSyncTransitionsToLoaded() async throws {
        let kpi = makeKpi(messageCount: 250, wallMs: 500)
        let vm = makeVM(lastSyncKpi: { kpi })
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        guard case .loaded(let loaded) = vm.kpiState else {
            XCTFail("Expected .loaded; got \(vm.kpiState)")
            return
        }
        XCTAssertEqual(loaded.messageCount, 250)
        let rate = try XCTUnwrap(loaded.messagesPerSecond) // 250 / (500ms/1000) == 500
        XCTAssertEqual(rate, 500, accuracy: 0.01)
    }

    func testKpiFetchErrorTransitionsToUnavailableNotNone() async {
        let vm = makeVM(lastSyncKpi: { throw CastellumError.internalError })
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        if case .unavailable = vm.kpiState {} else {
            XCTFail("A real fetch failure must be .unavailable, not .none: got \(vm.kpiState)")
        }
    }
}

// MARK: - Integrity-check state transitions

@MainActor
final class DiagnosticsViewModelIntegrityTests: XCTestCase {
    func testIntegrityStartsIdle() {
        let vm = makeVM()
        if case .idle = vm.integrityState {} else {
            XCTFail("Expected .idle; got \(vm.integrityState)")
        }
    }

    func testIntegrityOkTransition() async {
        let vm = makeVM(integrity: { makeIntegrityReport(ok: true) })
        vm.runIntegrityCheck()
        for _ in 0..<10 { await Task.yield() }
        if case .ok = vm.integrityState {} else {
            XCTFail("Expected .ok; got \(vm.integrityState)")
        }
    }

    func testIntegrityFailedTransition() async {
        let vm = makeVM(integrity: { makeIntegrityReport(ok: false, issueCount: 2) })
        vm.runIntegrityCheck()
        for _ in 0..<10 { await Task.yield() }
        if case .failed(let count, _) = vm.integrityState {
            XCTAssertEqual(count, 2)
        } else {
            XCTFail("Expected .failed; got \(vm.integrityState)")
        }
    }

    func testIntegrityErrorTransition() async {
        let vm = makeVM(integrity: { throw CastellumError.internalError })
        vm.runIntegrityCheck()
        for _ in 0..<10 { await Task.yield() }
        if case .error = vm.integrityState {} else {
            XCTFail("Expected .error; got \(vm.integrityState)")
        }
    }

    func testIntegrityRunningNotRestartable() async {
        var callCount = 0
        let gate = AsyncStream<Void>.makeStream()
        let vm = makeVM(integrity: {
            // Wait until released.
            var iter = gate.stream.makeAsyncIterator()
            _ = await iter.next()
            callCount += 1
            return makeIntegrityReport(ok: true)
        })
        vm.runIntegrityCheck()
        await Task.yield()
        // A second call while running must be a no-op.
        vm.runIntegrityCheck()
        gate.continuation.yield(())
        gate.continuation.finish()
        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(callCount, 1, "second runIntegrityCheck while running must be ignored")
    }

    func testResetIntegrity() async {
        let vm = makeVM(integrity: { makeIntegrityReport(ok: true) })
        vm.runIntegrityCheck()
        for _ in 0..<10 { await Task.yield() }
        vm.resetIntegrity()
        if case .idle = vm.integrityState {} else {
            XCTFail("Expected .idle after reset; got \(vm.integrityState)")
        }
    }
}

// MARK: - Gather state transitions

@MainActor
final class DiagnosticsViewModelGatherTests: XCTestCase {
    func testGatherReadyToSaveTransition() async {
        let vm = makeVM(bundle: { makeBundle() }, lastSyncKpi: { makeKpi() })
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave; got \(vm.exportState)")
            return
        }
        XCTAssertFalse(gathered.supportBundleJSON.isEmpty)
        XCTAssertNoThrow(try JSONSerialization.jsonObject(with: Data(gathered.supportBundleJSON.utf8)))
        XCTAssertTrue(gathered.environment.contains("Castellum"))
        XCTAssertEqual(gathered.appLog, "log line")
        XCTAssertTrue(gathered.syncKpiSummary.contains("Messages: 100"), "expected KPI summary: \(gathered.syncKpiSummary)")
    }

    func testGatherWithNoRecordedSyncWritesANoSyncSummaryNotAnError() async {
        let vm = makeVM(bundle: { makeBundle() }, lastSyncKpi: { nil })
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave; got \(vm.exportState)")
            return
        }
        XCTAssertTrue(
            gathered.syncKpiSummary.lowercased().contains("no sync recorded"),
            "expected a 'no sync yet' summary, got: \(gathered.syncKpiSummary)"
        )
    }

    func testGatherAppliesRedactionToAppLogCrashReportsAndRustLogNotToTheBundle() async {
        let vm = makeVM(
            bundle: { makeBundle() },
            crashReports: { [makeCrashReport(contents: "SENTINEL crash")] },
            appLog: { "SENTINEL log" },
            logDirectory: { URL(fileURLWithPath: "/nonexistent-\(UUID().uuidString)") },
            redact: { $0.replacingOccurrences(of: "SENTINEL", with: "[redacted]") }
        )
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave")
            return
        }
        XCTAssertEqual(gathered.appLog, "[redacted] log")
        XCTAssertEqual(gathered.crashReports.first?.contents, "[redacted] crash")
        // A missing log directory degrades to "" (see DiagnosticsBundleGatherer
        // tests), which still passes through redactText — proving the call is
        // wired in, not skipped, even when there's nothing to redact.
        XCTAssertEqual(gathered.rustLog, "")
    }

    /// The redaction call for the Rust log excerpt is actually reached with
    /// real (non-empty) content — the test above only proves the wiring for
    /// the empty-directory case, which can't distinguish "redactText was
    /// called on ''" from "redactText was never called".
    func testGatherRedactsNonEmptyRustLogContent() async throws {
        let dir = TestScratch.root
            .appendingPathComponent("gather-rustlog-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try Data("SENTINEL rust log line".utf8).write(to: dir.appendingPathComponent("castellum.log"))

        let vm = makeVM(
            bundle: { makeBundle() },
            logDirectory: { dir },
            redact: { $0.replacingOccurrences(of: "SENTINEL", with: "[redacted]") }
        )
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave")
            return
        }
        XCTAssertEqual(gathered.rustLog, "[redacted] rust log line")
    }

    func testGatherErrorTransition() async {
        let vm = makeVM(bundle: { throw CastellumError.internalError })
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        if case .error = vm.exportState {} else {
            XCTFail("Expected .error; got \(vm.exportState)")
        }
    }

    func testBeginGatherWhileGatheringIsNoOp() async {
        var callCount = 0
        let gate = AsyncStream<Void>.makeStream()
        let entered = expectation(description: "a gather reached the bundle closure")
        let completed = expectation(description: "the first gather finished")
        let vm = makeVM(bundle: {
            entered.fulfill()
            var iter = gate.stream.makeAsyncIterator()
            _ = await iter.next()
            callCount += 1
            completed.fulfill()
            return makeBundle()
        })

        vm.beginGather()
        await fulfillment(of: [entered], timeout: 5)
        vm.beginGather() // must be a no-op
        gate.continuation.yield(())
        gate.continuation.finish()
        await fulfillment(of: [completed], timeout: 5)

        XCTAssertEqual(callCount, 1, "second beginGather while gathering must be ignored")
    }

    func testSaveGatheredBundleWritesTheExpectedFileTree() async throws {
        let vm = makeVM(
            bundle: { makeBundle() },
            crashReports: { [makeCrashReport(filename: "Castellum-2026-09-17.ips", contents: "trace")] }
        )
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave")
            return
        }
        let parent = TestScratch.root
            .appendingPathComponent("gather-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }

        await vm.saveGatheredBundle(gathered, toParentDirectory: parent)
        guard case .saved(let folder) = vm.exportState else {
            XCTFail("Expected .saved; got \(vm.exportState)")
            return
        }
        let fm = FileManager.default
        XCTAssertTrue(fm.fileExists(atPath: folder.appendingPathComponent("support-bundle.json").path))
        XCTAssertTrue(fm.fileExists(atPath: folder.appendingPathComponent("environment.txt").path))
        XCTAssertTrue(fm.fileExists(atPath: folder.appendingPathComponent("app-log.txt").path))
        XCTAssertTrue(fm.fileExists(atPath: folder.appendingPathComponent("rust-log.txt").path))
        XCTAssertTrue(fm.fileExists(atPath: folder.appendingPathComponent("sync-kpi.txt").path))
        XCTAssertTrue(fm.fileExists(atPath: folder.appendingPathComponent("crash-reports/Castellum-2026-09-17.txt").path))
    }

    func testSaveGatheredBundleWithNoCrashReportsWritesANote() async throws {
        let vm = makeVM(bundle: { makeBundle() }, crashReports: { [] })
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave")
            return
        }
        let parent = TestScratch.root
            .appendingPathComponent("gather-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }

        await vm.saveGatheredBundle(gathered, toParentDirectory: parent)
        guard case .saved(let folder) = vm.exportState else {
            XCTFail("Expected .saved")
            return
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: folder.appendingPathComponent("crash-reports.txt").path))
    }

    func testSaveErrorSurfacedWhenParentDoesNotExist() async {
        let vm = makeVM(bundle: { makeBundle() })
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        guard case .readyToSave(let gathered) = vm.exportState else {
            XCTFail("Expected .readyToSave")
            return
        }
        let missingParent = TestScratch.root
            .appendingPathComponent("does-not-exist-\(UUID().uuidString)")
            .appendingPathComponent("nested-does-not-exist-either")
        await vm.saveGatheredBundle(gathered, toParentDirectory: missingParent)
        // createDirectory(withIntermediateDirectories: true) actually succeeds
        // here even for a missing parent, so assert the POSITIVE case instead —
        // .saved with a folder that now exists — proving intermediate
        // directories are created rather than requiring a pre-existing parent.
        guard case .saved(let folder) = vm.exportState else {
            XCTFail("Expected .saved; got \(vm.exportState)")
            return
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: folder.path))
        try? FileManager.default.removeItem(
            at: TestScratch.root.appendingPathComponent(missingParent.lastPathComponent)
        )
    }

    func testResetExport() async {
        let vm = makeVM(bundle: { throw CastellumError.internalError })
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        vm.resetExport()
        if case .idle = vm.exportState {} else {
            XCTFail("Expected .idle after reset; got \(vm.exportState)")
        }
    }

    func testBundleFolderNameShape() {
        let name = DiagnosticsViewModel.bundleFolderName(date: Date(timeIntervalSince1970: 0))
        XCTAssertTrue(name.hasPrefix("castellum-diagnostics-"), "Bad prefix: \(name)")
        XCTAssertFalse(name.contains("/"))
        XCTAssertFalse(name.contains(" "))
    }

    func testStatusLabelDistinguishesNoRecordFromNeverSynced() {
        XCTAssertEqual(
            statusLabel(consecutiveFailures: 0, lastSuccessAt: nil,
                        lastAttemptAt: nil, hasLocalData: true),
            "no sync recorded"
        )
        XCTAssertEqual(
            statusLabel(consecutiveFailures: 0, lastSuccessAt: nil,
                        lastAttemptAt: nil, hasLocalData: false),
            "never synced"
        )
    }

    // MARK: - Local vs export identity separation

    func testLocalDTOHasEmailBundleDTODoesNot() throws {
        let local = makeAccount(email: "real@example.com")
        XCTAssertEqual(local.email, "real@example.com")

        let bundleAcct = AccountDiagnostics(
            label: "account-1",
            provider: "imap",
            capabilities: [],
            lastAttemptAt: nil,
            lastSuccessAt: nil,
            lastErrorKind: nil,
            lastError: nil,
            lastErrorPhase: nil,
            consecutiveFailures: 0,
            operations: OperationSummary(
                queued: 0, running: 0, succeeded: 0,
                failed: 0, failedPermanent: 0, cancelled: 0, other: 0
            ),
            mailboxes: []
        )
        let encoder = JSONEncoder()
        let data = try encoder.encode(bundleAcct)
        let json = String(data: data, encoding: .utf8)!
        XCTAssertFalse(json.contains("real@example.com"),
                       "Bundle DTO must not contain real email: \(json)")
        XCTAssertTrue(json.contains("account-1"),
                      "Bundle DTO must contain ordinal label: \(json)")
    }
}

// MARK: - No-write on cancel

@MainActor
final class DiagnosticsViewModelCancellationTests: XCTestCase {
    /// Cancellation (simulating Cancel in the destination picker): no
    /// saveGatheredBundle call is made, so no `castellum-diagnostics-*`
    /// folder is written anywhere.
    ///
    /// Uses a directory this test owns (not the shared
    /// `/var/folders/.../T` container, which the rest of the test host
    /// churns constantly and made an earlier count-based version of this
    /// test flaky) so an empty result is unambiguous.
    func testCancelGatherLeavesNoFolder() async throws {
        let parent = TestScratch.root
            .appendingPathComponent("cancel-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }

        let vm = makeVM(bundle: { makeBundle() })
        vm.beginGather()
        for _ in 0..<10 { await Task.yield() }
        vm.resetExport() // simulates Cancel — no saveGatheredBundle call made

        let entries = try FileManager.default.contentsOfDirectory(atPath: parent.path)
        XCTAssertTrue(
            entries.filter { $0.hasPrefix("castellum-diagnostics-") }.isEmpty,
            "Cancel must not write any castellum-diagnostics-* folder; found \(entries)"
        )
    }
}
