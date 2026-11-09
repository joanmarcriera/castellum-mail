// Castellum - M3 (S64): model-level timings of the interactions a user feels
// after launch, against the seeded 100k profile — mailbox switch and message
// open, as far as they are reachable without a window.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// ADVISORY: these rows warn and never fail (budgets need Marc's sign-off), so
// each one is judged against a ceiling no sample can exceed in practice and a
// FAIL verdict is downgraded to a WARN line. Skips cleanly without
// CASTELLUM_PERF_PROFILE, like its siblings. The matching on-device signposts
// are in PerfSignpost.swift.

import Combine
import XCTest
@testable import Castellum

@MainActor
final class InteractionPerfTests: XCTestCase {
    private static let iterations = 20

    private func inbox(_ account: String) -> Mailbox {
        Mailbox(
            id: "\(account):INBOX", accountId: account, path: "INBOX", name: "INBOX",
            role: "inbox", delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    private static func milliseconds(_ d: Duration) -> Double {
        Double(d.components.seconds) * 1_000 + Double(d.components.attoseconds) / 1e15
    }

    /// Prints the PERF row for `name`. Advisory: the ceiling is only used for
    /// the WARN verdict, so a slow machine can never fail the build.
    private func report(_ name: String, target: Double, ceiling: Double, samples: [Double]) {
        let sorted = samples.sorted()
        func pct(_ p: Double) -> Double { sorted[Int((Double(sorted.count - 1) * p).rounded())] }
        let p50 = pct(0.5), p95 = pct(0.95)
        let verdict = p95 <= target ? "PASS" : "WARN"
        print(String(
            format: "PERF %@ p50=%.1fms p95=%.1fms target=%.0fms ceiling=%.0fms verdict=%@ (advisory) load=%.2f",
            name, p50, p95, target, ceiling, verdict, Self.loadAverage()))
        if p95 > ceiling { print("WARN \(name): p95 above the proposed ceiling (advisory, not a failure)") }
    }

    private static func loadAverage() -> Double {
        var loads = [Double](repeating: 0, count: 3)
        return getloadavg(&loads, 3) > 0 ? loads[0] : -1
    }

    /// 20 x `select(mailbox:)` / `select(unifiedRole:)` alternating between
    /// acct-a INBOX (45k) and the unified inbox: trigger -> the new page is
    /// published in `messages`, stamped inside a sink (never polled).
    /// Measures the same span as the `MailboxSwitch` signpost.
    func testMailboxSwitch() throws {
        let dir = try PerfProfile.cloneOrSkip()
        defer { try? FileManager.default.removeItem(at: dir) }
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }

        var armed = false
        var publishedAt: ContinuousClock.Instant?
        var published: XCTestExpectation?
        let sink = model.$messages.sink { rows in
            guard armed, publishedAt == nil, !rows.isEmpty else { return }
            publishedAt = .now
            published?.fulfill()
        }
        defer { sink.cancel() }

        var samples: [Double] = []
        for i in 0..<(Self.iterations + 1) {
            armed = true
            publishedAt = nil
            let reached = expectation(description: "list reloaded \(i)")
            published = reached
            let start = ContinuousClock.now
            if i.isMultiple(of: 2) { model.select(mailbox: inbox("acct-a")) }
            else { model.select(unifiedRole: "inbox") }
            wait(for: [reached], timeout: 10)
            armed = false
            guard let at = publishedAt else { return XCTFail("switch \(i) never published") }
            if i > 0 { samples.append(Self.milliseconds(at - start)) } // iteration 0 warms the connection
        }
        XCTAssertEqual(samples.count, Self.iterations)
        report("mailbox_switch_model", target: 16, ceiling: 50, samples: samples)
    }

    /// 20 x `showDetail(for:)` on distinct rows of the first page: selection
    /// -> `detail` published (the local body read). A seeded message without a
    /// stored body would start a network hydration; the profile has no
    /// credentials, so that fails fast and still leaves `detail` set.
    func testMessageOpen() throws {
        let dir = try PerfProfile.cloneOrSkip()
        defer { try? FileManager.default.removeItem(at: dir) }
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }

        let loaded = expectation(description: "first page")
        let listSink = model.$messages.sink { if $0.count >= 100 { loaded.fulfill() } }
        model.select(mailbox: inbox("acct-a"))
        wait(for: [loaded], timeout: 10)
        listSink.cancel()
        let rows = Array(model.messages.prefix(Self.iterations + 1))
        XCTAssertEqual(rows.count, Self.iterations + 1)

        var armed = false
        var openedAt: ContinuousClock.Instant?
        var opened: XCTestExpectation?
        let sink = model.$detail.sink { detail in
            guard armed, openedAt == nil, detail != nil else { return }
            openedAt = .now
            opened?.fulfill()
        }
        defer { sink.cancel() }

        var samples: [Double] = []
        for (i, row) in rows.enumerated() {
            armed = true
            openedAt = nil
            let reached = expectation(description: "detail \(i)")
            opened = reached
            let start = ContinuousClock.now
            model.showDetail(for: row)
            wait(for: [reached], timeout: 10)
            armed = false
            guard let at = openedAt else { return XCTFail("open \(i) never published") }
            if i > 0 { samples.append(Self.milliseconds(at - start)) } // row 0 warms the connection
        }
        XCTAssertEqual(samples.count, Self.iterations)
        report("message_open_model", target: 5, ceiling: 25, samples: samples)
    }
}
