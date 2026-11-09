// Castellum - S64 Task 6: the GUI's launch-time read path against the
// seeded 100k profile. Mirrors `read_perf_gate.rs`'s `launch_data_path`
// budget (row 2) but through the SWIFT wrappers, so the cost of decoding
// each JSON payload into `Codable` structs is included too.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class LaunchPathPerfTests: XCTestCase {
    /// 5 runs, each on a fresh clone and a fresh `CastellumCore`: time from
    /// before `init` to the first unified-inbox page decoded, through the
    /// same calls `AppModel.bootstrap()`'s local part makes. Judged on the
    /// M6: the contacts refresh is deliberately absent (the app runs it after
    /// the first list is delivered). Judged on the
    /// median (Task 5 row 2's own "(median)"), not p95 — five samples are too
    /// few for a meaningful p95.
    func testLaunchDataPath() throws {
        var samplesMs: [Double] = []
        for _ in 0..<5 {
            let dir = try PerfProfile.cloneOrSkip()
            defer { try? FileManager.default.removeItem(at: dir) }
            let start = Date()
            let core = try CastellumCore(profilePath: dir)
            let accounts = try core.listAccounts()
            for account in accounts {
                _ = try core.listMailboxes(accountId: account.id)
                _ = try core.mailboxCounts(accountId: account.id)
                _ = try core.listDrafts(accountId: account.id)
            }
            _ = try core.listSmartMailboxes()
            _ = try core.listSnoozedMessages(limit: 500, offset: 0)
            _ = try core.listMessagesPage(byRole: "inbox", after: nil, limit: 500)
            samplesMs.append(Date().timeIntervalSince(start) * 1000)
        }
        assertBudget(
            PerfBudget(name: "gui_launch_data_path", targetMs: 500, ceilingMs: 2_000),
            samplesMs: samplesMs, judgeOn: .p50)
    }
}
