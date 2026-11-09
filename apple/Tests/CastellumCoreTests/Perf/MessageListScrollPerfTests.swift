// Castellum - S64 Task 6: list-scroll budgets against the seeded 100k
// profile — paging in a new keyset page (Task 4's `loadMoreMessagesIfNeeded`)
// and configuring a screen's worth of cells once 10,000 rows are loaded.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import Combine
import XCTest
@testable import Castellum

@MainActor
final class MessageListScrollPerfTests: XCTestCase {
    private func acctAInboxMailbox() -> Mailbox {
        Mailbox(
            id: "acct-a:INBOX", accountId: "acct-a", path: "INBOX", name: "INBOX",
            role: "inbox", delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    /// A message-list table hosted the way `MessageListView.makeNSView`
    /// hosts it — 22 pt rows, the real columns and widths, inside an
    /// `NSScrollView` — in a borderless window that is never ordered front.
    /// The scroll view matters: a bare `NSTableView()` has no clip view, so
    /// every row counts as visible and each `insertRows` realised a view for
    /// every loaded row (10,500 by the last append).
    private func hostedTable(coordinator: MessageListView.Coordinator) -> (NSWindow, NSTableView) {
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 1_000, height: 880)) // ~40 rows
        let table = NSTableView(frame: scroll.bounds)
        table.rowHeight = 22
        table.style = .inset
        table.columnAutoresizingStyle = .noColumnAutoresizing
        for spec in kColumns {
            let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(spec.id))
            column.width = spec.width
            table.addTableColumn(column)
        }
        scroll.documentView = table
        let window = NSWindow(contentRect: scroll.frame, styleMask: [.borderless], backing: .buffered, defer: true)
        // A code-built NSWindow defaults to releasing itself on close, which
        // under ARC over-releases it (it crashed this test's autorelease pool).
        window.isReleasedWhenClosed = false
        window.contentView = scroll
        table.dataSource = coordinator
        table.delegate = coordinator
        return (window, table)
    }

    private static func milliseconds(_ d: Duration) -> Double {
        Double(d.components.seconds) * 1_000 + Double(d.components.attoseconds) / 1e15
    }

    /// 20 × `loadMoreMessagesIfNeeded`, each observed directly. Two budgets
    /// per append:
    /// - `page_append_wall`: trigger -> the appended `messages` is published
    ///   (off-main fetch + JSON decode + main-actor hop + publish), stamped
    ///   inside a `$messages` sink at the moment of publication. Not an
    ///   `XCTNSPredicateExpectation`: that polls about once a second, and it
    ///   WAS the ~1 s this budget used to report (S64 review round 3; on the
    ///   same 20 appends the predicate wait read p50 1,068 ms while the sink
    ///   read p50 18.5 ms).
    /// - `page_append_main`: time in `Coordinator.apply` for the append, on
    ///   a `hostedTable` scrolled to its last row, as a user loading more is
    ///   (~80 row views realised, as in the app).
    func testPageAppend() throws {
        let dir = try PerfProfile.cloneOrSkip()
        defer { try? FileManager.default.removeItem(at: dir) }
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }

        // Fulfilled from the publication itself, never polled.
        var wantCount = Int.max
        var publishedAt: ContinuousClock.Instant?
        var published: XCTestExpectation?
        let sink = model.$messages.sink { rows in
            guard publishedAt == nil, rows.count >= wantCount else { return }
            publishedAt = .now
            published?.fulfill()
        }
        defer { sink.cancel() }
        /// Runs `trigger`, waits for `messages` to reach `count` rows, and
        /// returns trigger -> publication in ms.
        func publishTime(atLeast count: Int, _ trigger: () -> Void) -> Double? {
            wantCount = count
            publishedAt = nil
            let reached = expectation(description: "messages reached \(count) rows")
            published = reached
            let start = ContinuousClock.now
            trigger()
            wait(for: [reached], timeout: 10)
            return publishedAt.map { Self.milliseconds($0 - start) }
        }

        _ = publishTime(atLeast: 500) { model.select(mailbox: acctAInboxMailbox()) }
        XCTAssertEqual(model.messages.count, 500, "first page")

        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        let (window, table) = hostedTable(coordinator: coord)
        // The window owns the scroll view that makes this table "hosted";
        // Swift may release an unused local early, so pin it to the scope.
        defer { withExtendedLifetime(window) {} }
        XCTAssertNotNil(table.enclosingScrollView)
        coord.rows = model.messages
        table.reloadData()

        var wallSamplesMs: [Double] = []
        var mainSamplesMs: [Double] = []
        for _ in 0..<20 {
            let expected = model.messages.count + Int(AppModel.listPageSize)
            guard let wall = publishTime(atLeast: expected, {
                model.loadMoreMessagesIfNeeded(lastVisibleRow: model.messages.count - 1)
            }) else { return XCTFail("page \(wallSamplesMs.count + 1) never appended") }
            wallSamplesMs.append(wall)

            table.scrollRowToVisible(table.numberOfRows - 1)
            let mainStart = ContinuousClock.now
            coord.apply(model: model, table: table)
            mainSamplesMs.append(Self.milliseconds(.now - mainStart))
            XCTAssertEqual(table.numberOfRows, model.messages.count)
        }
        XCTAssertEqual(wallSamplesMs.count, 20, "acct-a INBOX (45k) has 20 more pages")
        // Guards the hosting itself: a bare table realises every loaded row
        // (10,500 here). Hosted, it holds a few screens' worth (82 to 246
        // measured; with no display pass between appends, views of rows
        // scrolled past are not recycled yet).
        var realised = 0
        table.enumerateAvailableRowViews { _, _ in realised += 1 }
        XCTAssertLessThan(
            realised, 1_000,
            "a hosted table realises a few screens of rows, not all \(table.numberOfRows)")

        // Budgets from the direct measurement above, on this machine
        // (read-path-budgets.md, "Swift perf XCTests"): p95 over three runs was
        // 29.5 / 51.8 / 75.8 ms (wall) and 14.4 / 17.6 / 23.5 ms (main) at load
        // averages of ~20, ~86 and ~100 on 15 cores. Ceilings are 2x the worst
        // p95, rounded down; targets sit at the calm run's p95 (main: one 60 Hz
        // frame). They replace round 2's 2,200 ms / 700 ms, which were widened
        // to fit the predicate poll and bare-table artefacts described above.
        assertBudget(PerfBudget(name: "page_append_wall", targetMs: 30, ceilingMs: 150), samplesMs: wallSamplesMs)
        assertBudget(PerfBudget(name: "page_append_main", targetMs: 16, ceilingMs: 45), samplesMs: mainSamplesMs)
    }

    /// With 10,000 rows loaded directly (bypassing paging — this budget is
    /// about per-cell configuration cost, not fetch), request
    /// `tableView(_:viewFor:row:)` for every column of 40 consecutive rows
    /// (one screen at 22pt row height on a typical window) at 25 positions
    /// spread down the list.
    func testCellConfiguration() throws {
        let dir = try PerfProfile.cloneOrSkip()
        defer { try? FileManager.default.removeItem(at: dir) }
        let core = try CastellumCore(profilePath: dir)
        let rows = try core.listMessages(accountId: "acct-a", mailboxId: "acct-a:INBOX", limit: 10_000, offset: 0)
        XCTAssertGreaterThanOrEqual(rows.count, 10_000, "acct-a INBOX must have at least 10k seeded messages")

        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }
        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        coord.rows = rows
        let table = NSTableView()
        var columns: [NSTableColumn] = []
        for spec in kColumns {
            let c = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(spec.id))
            table.addTableColumn(c)
            columns.append(c)
        }

        let screenRows = 40
        let positions = (0..<25).map { $0 * (rows.count - screenRows) / 24 }
        var samplesMs: [Double] = []
        for start in positions {
            let begin = Date()
            for row in start..<(start + screenRows) {
                for column in columns { _ = coord.tableView(table, viewFor: column, row: row) }
            }
            samplesMs.append(Date().timeIntervalSince(begin) * 1000)
        }
        // Ceiling 20 ms (was 12): 1.4x the worst p95 ever measured (14.5 ms at
        // load ~100) and below 2x a calm CI p95 (10.9 ms), so a doubled cell
        // cost still fails. The old 12 ms sat ~1.1x above the calm p95 and
        // failed on 7f50973e and bd2dc0c3 (p95 12.4 / 12.8 ms) when every other
        // row of the same runs, gui_launch_data_path included, was ~10% slower:
        // machine load, not a cell regression. CI also re-runs a failed perf
        // step once (scripts/ci/perf-retry.sh). The 4 ms target (WARN) stays.
        assertBudget(PerfBudget(name: "cells_per_screen", targetMs: 4, ceilingMs: 20), samplesMs: samplesMs)
    }
}
