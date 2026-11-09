// Castellum - DiagnosticsBundleGatherer unit tests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Every test uses a temp directory or a nonexistent executable path — never
// the real ~/Library/Logs/DiagnosticReports and never the real /usr/bin/log.

import XCTest
@testable import Castellum

final class DiagnosticsBundleGathererCrashReportTests: XCTestCase {
    private func tempDir() -> URL {
        let url = TestScratch.root
            .appendingPathComponent("castellum-crash-test-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    private func write(_ name: String, contents: String, at dir: URL, modified: Date = Date()) {
        let url = dir.appendingPathComponent(name)
        try! Data(contents.utf8).write(to: url)
        try! FileManager.default.setAttributes([.modificationDate: modified], ofItemAtPath: url.path)
    }

    func testOnlyCastellumPrefixedReportsAreIncluded() {
        let dir = tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        write("Castellum-2026-09-17-100000.ips", contents: "castellum crash", at: dir)
        write("Mail-2026-09-17-100000.ips", contents: "mail crash SENTINEL", at: dir)
        write("CastellumHelper-2026-09-17-100000.ips", contents: "unrelated helper", at: dir)

        let reports = DiagnosticsBundleGatherer.recentCrashReports(in: dir)
        XCTAssertEqual(reports.count, 1)
        XCTAssertEqual(reports.first?.filename, "Castellum-2026-09-17-100000.ips")
    }

    func testCapsToTheMostRecentReports() {
        let dir = tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        for i in 0..<8 {
            write(
                "Castellum-report-\(i).ips", contents: "report \(i)",
                at: dir, modified: Date().addingTimeInterval(Double(i))
            )
        }
        let reports = DiagnosticsBundleGatherer.recentCrashReports(in: dir)
        XCTAssertEqual(reports.count, DiagnosticsBundleGatherer.maxCrashReports)
        XCTAssertEqual(reports.first?.filename, "Castellum-report-7.ips", "newest first")
    }

    func testExcludesReportsOlderThanTheAgeCutoff() {
        let dir = tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(
            "Castellum-old.ips", contents: "old", at: dir,
            modified: Date().addingTimeInterval(-40 * 86_400)
        )
        write("Castellum-recent.ips", contents: "recent", at: dir)
        let reports = DiagnosticsBundleGatherer.recentCrashReports(in: dir)
        XCTAssertEqual(reports.map(\.filename), ["Castellum-recent.ips"])
    }

    func testMissingDirectoryReturnsEmptyNotAnError() {
        let missing = TestScratch.root
            .appendingPathComponent("does-not-exist-\(UUID().uuidString)")
        XCTAssertEqual(DiagnosticsBundleGatherer.recentCrashReports(in: missing), [])
    }

    func testTruncatesOversizedReports() {
        let dir = tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let big = String(repeating: "x", count: DiagnosticsBundleGatherer.maxCrashReportBytes + 1000)
        write("Castellum-big.ips", contents: big, at: dir)
        let reports = DiagnosticsBundleGatherer.recentCrashReports(in: dir)
        XCTAssertEqual(reports.count, 1)
        XCTAssertLessThan(reports[0].contents.utf8.count, big.utf8.count)
        XCTAssertTrue(reports[0].contents.contains("truncated"))
    }
}

final class DiagnosticsBundleGathererAppLogTests: XCTestCase {
    func testLogShowArgumentsScopeToThisProcessOnly() {
        let args = DiagnosticsBundleGatherer.logShowArguments(processName: "Castellum", lookback: "1h")
        XCTAssertEqual(
            args,
            ["show", "--predicate", "process == \"Castellum\"", "--style", "compact", "--last", "1h"]
        )
    }

    func testRecentAppLogReturnsEmptyStringWhenTheLogBinaryIsMissing() {
        let out = DiagnosticsBundleGatherer.recentAppLog(
            executableURL: URL(fileURLWithPath: "/nonexistent/log")
        )
        XCTAssertEqual(out, "", "a missing/failing subprocess must degrade to empty, never throw or hang")
    }
}

// MARK: - Rust log excerpt

final class DiagnosticsBundleGathererRustLogTests: XCTestCase {
    private func tempDir() -> URL {
        let url = TestScratch.root
            .appendingPathComponent("castellum-rustlog-test-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    func testNilDirectoryReturnsEmptyNotAnError() {
        XCTAssertEqual(DiagnosticsBundleGatherer.recentRustLog(in: nil), "")
    }

    func testMissingDirectoryReturnsEmptyNotAnError() {
        let missing = TestScratch.root
            .appendingPathComponent("does-not-exist-\(UUID().uuidString)")
        XCTAssertEqual(DiagnosticsBundleGatherer.recentRustLog(in: missing), "")
    }

    func testEmptyDirectoryReturnsEmptyNotAnError() {
        let dir = tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        XCTAssertEqual(DiagnosticsBundleGatherer.recentRustLog(in: dir), "")
    }

    func testReturnsTheNewestFilesFullContentsWhenUnderTheCap() throws {
        let dir = tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let older = dir.appendingPathComponent("castellum.2026-09-20.log")
        let newer = dir.appendingPathComponent("castellum.2026-09-23.log")
        try Data("older day's log".utf8).write(to: older)
        try FileManager.default.setAttributes(
            [.modificationDate: Date().addingTimeInterval(-86_400)], ofItemAtPath: older.path
        )
        try Data("today's log — most recent".utf8).write(to: newer)
        try FileManager.default.setAttributes([.modificationDate: Date()], ofItemAtPath: newer.path)

        let out = DiagnosticsBundleGatherer.recentRustLog(in: dir)
        XCTAssertEqual(out, "today's log — most recent", "must pick the most-recently-modified file, not the oldest/alphabetically-first")
    }

    /// The load-bearing case: unlike `recentAppLog`'s `data.prefix(...)` (kept
    /// OLDEST bytes, safe there only because `log show --last` already scopes
    /// its own output to a bounded recent window), a Rust log file is plain
    /// chronological/append-only and can exceed the cap — so the excerpt must
    /// keep the NEWEST bytes (the tail), not the oldest, or "what happened
    /// right before this went wrong" would be silently cut off.
    func testOversizedFileKeepsTheTailNotTheHead() throws {
        let dir = tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let cap = DiagnosticsBundleGatherer.maxRustLogBytes
        // The head marker sits well outside the retained window: filler alone
        // is 2x the cap, so a correct (tail-keeping) truncation can never
        // reach back far enough to include it. A head-keeping truncation
        // (the wrong direction) would keep the marker immediately.
        let headMarker = "HEAD_MARKER_SHOULD_BE_DROPPED"
        let filler = String(repeating: "x", count: cap * 2)
        let tailMarker = "TAIL_MARKER_SHOULD_SURVIVE"
        let file = dir.appendingPathComponent("castellum.log")
        try Data((headMarker + filler + tailMarker).utf8).write(to: file)

        let out = DiagnosticsBundleGatherer.recentRustLog(in: dir)
        XCTAssertTrue(out.hasSuffix(tailMarker), "expected the tail marker to survive truncation: \(out.suffix(80))")
        XCTAssertFalse(out.contains(headMarker), "the head of an oversized file must be dropped, not kept")
        XCTAssertTrue(out.contains("truncated"), "an oversized excerpt should say so")
    }
}
