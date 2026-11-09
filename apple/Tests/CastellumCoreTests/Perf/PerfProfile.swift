// Castellum - S64 Task 6: shared perf-test scaffolding — locating the seeded
// 100k profile and judging a sample against a two-tier budget, the Swift
// mirror of `perf_common::Budget`/`perf_line`
// (native/crates/castellum-core/examples/perf_common/mod.rs).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation
import XCTest

/// Locates the seeded profile for perf tests. `xcodebuild` forwards
/// `TEST_RUNNER_CASTELLUM_PERF_PROFILE` to the test process as
/// `CASTELLUM_PERF_PROFILE`. Unset -> the test is skipped, so the normal
/// `scripts/build-macos.sh test` run never pays for these (Task 6 Step 1).
enum PerfProfile {
    /// Clones the fixture into a fresh temp directory (an APFS clone via
    /// `FileManager.copyItem` — O(1) regardless of the 100k profile's size,
    /// same trick `read_perf_gate.rs` uses on the Rust side) so repeated
    /// runs never mutate the shared fixture on disk.
    static func cloneOrSkip(file: StaticString = #filePath, line: UInt = #line) throws -> URL {
        guard let src = ProcessInfo.processInfo.environment["CASTELLUM_PERF_PROFILE"],
              FileManager.default.fileExists(atPath: src + "/castellum.db")
        else { throw XCTSkip("CASTELLUM_PERF_PROFILE not set — perf budgets skipped") }
        let dst = TestScratch.root
            .appendingPathComponent("castellum-perf-\(UUID().uuidString)")
        try FileManager.default.copyItem(at: URL(fileURLWithPath: src), to: dst)
        return dst
    }
}

/// One perf budget in milliseconds — the Swift mirror of
/// `perf_common::Budget`. `target` is advisory (comfortable), `ceiling` is
/// blocking (an `XCTFail`); between the two is a printed `WARN`, never a
/// failure.
struct PerfBudget {
    let name: String
    let targetMs: Double
    let ceilingMs: Double
}

/// Which statistic of `samplesMs` a budget is judged against. The plan
/// judges `LaunchPathPerfTests` on the median (few, expensive samples: 5
/// full app-launch-shaped passes) and the scroll/cell budgets on p95 (many
/// cheap samples, and a p95 spike is exactly what a real scroll would feel).
enum JudgeStatistic { case p50, p95 }

private func percentile(_ sorted: [Double], _ p: Double) -> Double {
    guard !sorted.isEmpty else { return 0 }
    let idx = Int((Double(sorted.count - 1) * p).rounded())
    return sorted[idx]
}

/// Prints one `PERF <name> p50=…ms p95=…ms target=…ms ceiling=…ms
/// verdict=PASS|WARN|FAIL` line (same shape as `perf_common::perf_line`, so
/// the two are grep-compatible in a combined report) and `XCTFail`s only
/// when the judged statistic is above the ceiling.
@discardableResult
func assertBudget(
    _ budget: PerfBudget, samplesMs: [Double], judgeOn: JudgeStatistic = .p95,
    file: StaticString = #filePath, line: UInt = #line
) -> String {
    guard !samplesMs.isEmpty else {
        XCTFail("\(budget.name): no samples collected", file: file, line: line)
        return "FAIL"
    }
    let sorted = samplesMs.sorted()
    let p50 = percentile(sorted, 0.50)
    let p95 = percentile(sorted, 0.95)
    let judged = judgeOn == .p50 ? p50 : p95
    let verdict: String
    if judged <= budget.targetMs { verdict = "PASS" }
    else if judged <= budget.ceilingMs { verdict = "WARN" }
    else { verdict = "FAIL" }
    print(String(
        format: "PERF %@ p50=%.1fms p95=%.1fms target=%.0fms ceiling=%.0fms verdict=%@",
        budget.name, p50, p95, budget.targetMs, budget.ceilingMs, verdict))
    if verdict == "FAIL" {
        XCTFail("\(budget.name): \(String(format: "%.1f", judged))ms over the \(Int(budget.ceilingMs))ms ceiling", file: file, line: line)
    }
    return verdict
}
