// Castellum - periodic background re-sync scheduling tests:
// interval, backoff, and in-flight skip, driven by an injected clock so none
// of this waits out the real 5/15-minute windows.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class PeriodicSyncSchedulerTests: XCTestCase {
    private let epoch = Date(timeIntervalSince1970: 1_000_000)

    func testNeverAttemptedIsAlwaysDue() {
        let state = PeriodicSyncScheduler.AccountState(
            lastAttempt: nil, isUnreachable: false, isSyncing: false, idleActive: false)
        XCTAssertTrue(PeriodicSyncScheduler.isDue(state, now: epoch))
    }

    func testNormalIntervalIsFiveMinutes() {
        let state = PeriodicSyncScheduler.AccountState(
            lastAttempt: epoch, isUnreachable: false, isSyncing: false, idleActive: false)

        XCTAssertFalse(PeriodicSyncScheduler.isDue(
            state, now: epoch.addingTimeInterval(PeriodicSyncScheduler.normalInterval - 1)))
        XCTAssertTrue(PeriodicSyncScheduler.isDue(
            state, now: epoch.addingTimeInterval(PeriodicSyncScheduler.normalInterval)))
        XCTAssertTrue(PeriodicSyncScheduler.isDue(
            state, now: epoch.addingTimeInterval(PeriodicSyncScheduler.normalInterval + 60)))
    }

    func testUnreachableBacksOffToFifteenMinutes() {
        let state = PeriodicSyncScheduler.AccountState(
            lastAttempt: epoch, isUnreachable: true, isSyncing: false, idleActive: false)

        // Would be due under the normal 5-minute interval, but isn't while
        // the account is unreachable.
        XCTAssertFalse(PeriodicSyncScheduler.isDue(
            state, now: epoch.addingTimeInterval(PeriodicSyncScheduler.normalInterval + 60)))
        XCTAssertFalse(PeriodicSyncScheduler.isDue(
            state, now: epoch.addingTimeInterval(PeriodicSyncScheduler.backoffInterval - 1)))
        XCTAssertTrue(PeriodicSyncScheduler.isDue(
            state, now: epoch.addingTimeInterval(PeriodicSyncScheduler.backoffInterval)))
    }

    func testInFlightSyncIsNeverDueRegardlessOfTiming() {
        let neverAttempted = PeriodicSyncScheduler.AccountState(
            lastAttempt: nil, isUnreachable: false, isSyncing: true, idleActive: false)
        XCTAssertFalse(PeriodicSyncScheduler.isDue(neverAttempted, now: epoch))

        let longOverdueButSyncing = PeriodicSyncScheduler.AccountState(
            lastAttempt: epoch.addingTimeInterval(-3600), isUnreachable: true, isSyncing: true, idleActive: false)
        XCTAssertFalse(PeriodicSyncScheduler.isDue(longOverdueButSyncing, now: epoch))
    }

    func testRecoveringFromUnreachableReturnsToTheNormalInterval() {
        // Last attempt failed 6 minutes ago (would be due normally, not yet
        // under backoff) but the account is no longer flagged unreachable —
        // e.g. it just recovered on a manual retry.
        let state = PeriodicSyncScheduler.AccountState(
            lastAttempt: epoch.addingTimeInterval(-(PeriodicSyncScheduler.normalInterval + 60)),
            isUnreachable: false, isSyncing: false, idleActive: false)
        XCTAssertTrue(PeriodicSyncScheduler.isDue(state, now: epoch))
    }

    func testIdleActiveAccountUsesTheSafetyNetIntervalNotTheNormalOne() {
        let sixMinutesAgo = epoch.addingTimeInterval(-6 * 60)
        let state = PeriodicSyncScheduler.AccountState(
            lastAttempt: sixMinutesAgo, isUnreachable: false, isSyncing: false, idleActive: true)
        // Past the normal 5-minute interval but well under the 15-minute
        // safety net — IDLE is presumed to be keeping it fresh.
        XCTAssertFalse(PeriodicSyncScheduler.isDue(state, now: epoch))
    }

    func testIdleActiveAccountStillPollsEventuallyAsASafetyNet() {
        let sixteenMinutesAgo = epoch.addingTimeInterval(-16 * 60)
        let state = PeriodicSyncScheduler.AccountState(
            lastAttempt: sixteenMinutesAgo, isUnreachable: false, isSyncing: false, idleActive: true)
        XCTAssertTrue(PeriodicSyncScheduler.isDue(state, now: epoch))
    }
}
