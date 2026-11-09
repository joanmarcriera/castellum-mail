// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Reusable async latch: `wait()` suspends until `open()`; already-open gates
/// pass through immediately.
private actor Gate {
    private var opened = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func open() {
        opened = true
        for waiter in waiters { waiter.resume() }
        waiters.removeAll()
    }

    func wait() async {
        if opened { return }
        await withCheckedContinuation { waiters.append($0) }
    }
}

/// Records how many operations are inside a critical section at once.
private actor ConcurrencyTracker {
    private var current = 0
    private(set) var maxConcurrent = 0
    private(set) var entries = 0

    func enter() {
        current += 1
        entries += 1
        maxConcurrent = max(maxConcurrent, current)
    }

    func exit() { current -= 1 }
}

/// Serves scripted hydration reports pass by pass and counts calls.
private actor ReportScript {
    private var reports: [HydrationReport?]
    private(set) var calls = 0

    init(_ reports: [HydrationReport?]) { self.reports = reports }

    func next() -> HydrationReport? {
        calls += 1
        precondition(!reports.isEmpty, "drain ran more passes than scripted")
        return reports.removeFirst()
    }
}

/// Records the byte budget passed into each scripted pass.
private actor BudgetRecorder {
    private(set) var values: [Int64] = []
    func record(_ value: Int64) { values.append(value) }
}

private func report(hydrated: Int, remaining: Int64, bytes: Int64 = 0) -> HydrationReport {
    HydrationReport(
        accountId: "acct",
        mailboxes: [],
        requested: hydrated,
        hydrated: hydrated,
        bytesFetched: bytes,
        remaining: remaining,
        elapsedMs: 1
    )
}

final class BackgroundHydrationLoopTests: XCTestCase {
    func testStopsWhenBacklogDrained() async {
        let script = ReportScript([report(hydrated: 5, remaining: 0)])
        let progress = ConcurrencyTracker()

        let stop = await BackgroundHydrationLoop.drain(
            runPass: { _ in await script.next() },
            onProgress: { await progress.enter() },
            betweenPasses: { XCTFail("drained pass must not sleep") }
        )

        XCTAssertEqual(stop, .drained)
        let calls = await script.calls
        let refreshes = await progress.entries
        XCTAssertEqual(calls, 1)
        XCTAssertEqual(refreshes, 1)
    }

    func testStopsOnZeroProgressPassWithoutRefreshing() async {
        let script = ReportScript([report(hydrated: 0, remaining: 10)])
        let progress = ConcurrencyTracker()

        let stop = await BackgroundHydrationLoop.drain(
            runPass: { _ in await script.next() },
            onProgress: { await progress.enter() },
            betweenPasses: {}
        )

        XCTAssertEqual(stop, .zeroProgress)
        let refreshes = await progress.entries
        XCTAssertEqual(refreshes, 0)
    }

    func testStopsOnFailedPass() async {
        let script = ReportScript([nil])

        let stop = await BackgroundHydrationLoop.drain(
            runPass: { _ in await script.next() },
            onProgress: { XCTFail("failed pass hydrated nothing") },
            betweenPasses: {}
        )

        XCTAssertEqual(stop, .failedPass)
    }

    func testStopsAtPassBudget() async {
        let script = ReportScript(
            Array(
                repeating: report(hydrated: 1, remaining: 100),
                count: BackgroundHydrationLoop.maxPassesPerSync
            )
        )
        let sleeps = ConcurrencyTracker()

        let stop = await BackgroundHydrationLoop.drain(
            runPass: { _ in await script.next() },
            onProgress: {},
            betweenPasses: { await sleeps.enter() }
        )

        XCTAssertEqual(stop, .passBudget)
        let calls = await script.calls
        let sleptBetween = await sleeps.entries
        XCTAssertEqual(calls, BackgroundHydrationLoop.maxPassesPerSync)
        // No sleep after the final pass.
        XCTAssertEqual(sleptBetween, BackgroundHydrationLoop.maxPassesPerSync - 1)
    }

    func testStopsAtByteBudgetAndLeavesBacklogForNextSync() async {
        let halfBudget = BackgroundHydrationLoop.maxBytesPerSync / 2
        let script = ReportScript([
            report(hydrated: 10, remaining: 50, bytes: halfBudget),
            report(hydrated: 10, remaining: 40, bytes: halfBudget),
        ])

        let stop = await BackgroundHydrationLoop.drain(
            runPass: { _ in await script.next() },
            onProgress: {},
            betweenPasses: {}
        )

        XCTAssertEqual(stop, .byteBudget)
        let calls = await script.calls
        XCTAssertEqual(calls, 2)
    }

    func testThreadsShrinkingRemainingByteBudgetIntoEachPass() async {
        let quarter = BackgroundHydrationLoop.maxBytesPerSync / 4
        let script = ReportScript([
            report(hydrated: 10, remaining: 50, bytes: quarter),
            report(hydrated: 10, remaining: 40, bytes: quarter),
            report(hydrated: 10, remaining: 0, bytes: quarter),
        ])
        let budgets = BudgetRecorder()

        let stop = await BackgroundHydrationLoop.drain(
            runPass: { remaining in
                await budgets.record(remaining)
                return await script.next()
            },
            onProgress: {},
            betweenPasses: {}
        )

        XCTAssertEqual(stop, .drained)
        let seen = await budgets.values
        XCTAssertEqual(
            seen,
            [
                BackgroundHydrationLoop.maxBytesPerSync,
                BackgroundHydrationLoop.maxBytesPerSync - quarter,
                BackgroundHydrationLoop.maxBytesPerSync - 2 * quarter,
            ],
            "each pass must receive the byte budget still unspent this sync"
        )
    }

    func testConfigBudgetInjectionPreservesStoredFields() throws {
        let stored = #"{"account_id":"a1","host":"imap.example.test","port":993}"#
        let injected = try XCTUnwrap(
            HydrationConfig.withByteBudget(stored, remainingBytes: 12_345)
        )
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(
                with: Data(injected.utf8)) as? [String: Any]
        )
        XCTAssertEqual(object["hydration_byte_budget"] as? Int64, 12_345)
        XCTAssertEqual(object["account_id"] as? String, "a1")
        XCTAssertEqual(object["host"] as? String, "imap.example.test")
        XCTAssertEqual(object["port"] as? Int, 993)

        XCTAssertNil(
            HydrationConfig.withByteBudget("not json", remainingBytes: 1),
            "unparseable stored config must fail the pass, not fetch unbudgeted"
        )

        // A stale budget already present in the stored config must be
        // replaced, never trusted over the actual remaining budget.
        let stale = #"{"account_id":"a1","hydration_byte_budget":99999999999}"#
        let replaced = try XCTUnwrap(
            HydrationConfig.withByteBudget(stale, remainingBytes: 7)
        )
        let replacedObject = try XCTUnwrap(
            JSONSerialization.jsonObject(
                with: Data(replaced.utf8)) as? [String: Any]
        )
        XCTAssertEqual(replacedObject["hydration_byte_budget"] as? Int64, 7)
    }

    func testStopsWhenCancelledWithoutRunningAnotherPass() async {
        let sleeping = Gate()
        let release = Gate()
        let script = ReportScript([
            report(hydrated: 1, remaining: 10),
            report(hydrated: 1, remaining: 9),
        ])

        let task = Task {
            await BackgroundHydrationLoop.drain(
                runPass: { _ in await script.next() },
                onProgress: {},
                betweenPasses: {
                    await sleeping.open()
                    await release.wait()
                }
            )
        }

        await sleeping.wait() // first pass done; drain is between passes
        task.cancel()
        await release.open()

        let stop = await task.value
        XCTAssertEqual(stop, .cancelled)
        let calls = await script.calls
        XCTAssertEqual(calls, 1, "cancelled drain must not start another pass")
    }
}

@MainActor
final class SerializedTaskSlotsTests: XCTestCase {
    func testReplacementWaitsForInFlightPredecessorPass() async {
        let slots = SerializedTaskSlots()
        let tracker = ConcurrencyTracker()
        let firstEntered = Gate()
        let releaseFirst = Gate()
        let secondDone = Gate()

        // Simulates a blocking, non-cancellation-aware FFI pass in flight.
        slots.replace(key: "acct") {
            await tracker.enter()
            await firstEntered.open()
            await releaseFirst.wait()
            await tracker.exit()
        }
        await firstEntered.wait()

        slots.replace(key: "acct") {
            await tracker.enter()
            await tracker.exit()
            await secondDone.open()
        }

        // Give an (incorrect) overlapping replacement time to start.
        try? await Task.sleep(nanoseconds: 100_000_000)
        let entriesWhileBlocked = await tracker.entries
        XCTAssertEqual(
            entriesWhileBlocked, 1,
            "replacement must not start while the predecessor's pass is in flight"
        )

        await releaseFirst.open()
        await secondDone.wait()

        let maxConcurrent = await tracker.maxConcurrent
        let entries = await tracker.entries
        XCTAssertEqual(maxConcurrent, 1)
        XCTAssertEqual(entries, 2, "the replacement still runs after the drain")
    }

    func testReplacementChainsAcrossThreeGenerations() async {
        let slots = SerializedTaskSlots()
        let tracker = ConcurrencyTracker()
        let firstEntered = Gate()
        let releaseFirst = Gate()
        let thirdDone = Gate()

        slots.replace(key: "acct") {
            await tracker.enter()
            await firstEntered.open()
            await releaseFirst.wait()
            await tracker.exit()
        }
        await firstEntered.wait()

        // Replace the replacement while generation 1 is still in flight.
        slots.replace(key: "acct") {
            await tracker.enter()
            await tracker.exit()
        }
        slots.replace(key: "acct") {
            await tracker.enter()
            await tracker.exit()
            await thirdDone.open()
        }

        try? await Task.sleep(nanoseconds: 100_000_000)
        let entriesWhileBlocked = await tracker.entries
        XCTAssertEqual(entriesWhileBlocked, 1, "no generation may start while gen 1 is in flight")

        await releaseFirst.open()
        await thirdDone.wait()

        let maxConcurrent = await tracker.maxConcurrent
        let entries = await tracker.entries
        XCTAssertEqual(maxConcurrent, 1, "chained replacements must never overlap")
        XCTAssertEqual(entries, 3, "every generation still runs in order")
    }

    func testIndependentKeysRunConcurrently() async {
        let slots = SerializedTaskSlots()
        let firstEntered = Gate()
        let releaseFirst = Gate()
        let secondDone = Gate()

        slots.replace(key: "a") {
            await firstEntered.open()
            await releaseFirst.wait()
        }
        await firstEntered.wait()

        // A different account's drain must not queue behind account "a".
        slots.replace(key: "b") { await secondDone.open() }
        await secondDone.wait()
        await releaseFirst.open()
    }
}
