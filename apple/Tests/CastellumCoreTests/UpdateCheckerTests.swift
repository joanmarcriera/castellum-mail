// Castellum - semantic-version and daily update schedule tests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class UpdateCheckerTests: XCTestCase {
    func testSemanticVersionAcceptsVPrefixAndTwoOrThreeParts() {
        XCTAssertEqual(SemanticVersion("v1.2"), SemanticVersion("1.2.0"))
        XCTAssertEqual(SemanticVersion("V10.20.30"), SemanticVersion("10.20.30"))
        XCTAssertNil(SemanticVersion("1"))
        XCTAssertNil(SemanticVersion("1.2.3.4"))
        XCTAssertNil(SemanticVersion("v1.two.3"))
    }

    func testSemanticComparisonUsesMajorMinorAndPatch() {
        XCTAssertTrue(UpdateChecker.isRemoteVersion("v0.3.2", newerThan: "0.3.1"))
        XCTAssertTrue(UpdateChecker.isRemoteVersion("v1.0", newerThan: "0.9.99"))
        XCTAssertFalse(UpdateChecker.isRemoteVersion("v0.3.1", newerThan: "0.3.1"))
        XCTAssertFalse(UpdateChecker.isRemoteVersion("v0.3", newerThan: "0.3.0"))
    }

    func testNewerLocalDevelopmentBuildDoesNotOfferOlderRelease() {
        XCTAssertFalse(UpdateChecker.isRemoteVersion("v0.3.1", newerThan: "0.4.0"))
        XCTAssertFalse(UpdateChecker.isRemoteVersion("v1.9.9", newerThan: "2.0"))
    }

    func testNeverScheduleNeverPerformsTheAutomaticCheck() {
        let current = Date(timeIntervalSince1970: 2_000_000)
        let checker = UpdateChecker(now: { current })

        XCTAssertFalse(checker.shouldPerformAutomaticCheck(schedule: .never, lastCheck: nil))
        XCTAssertFalse(checker.shouldPerformAutomaticCheck(
            schedule: .never,
            lastCheck: current.addingTimeInterval(-UpdateChecker.automaticInterval * 2)
        ))
    }

    func testAtLaunchSchedulePerformsEveryLaunchWithNoTwentyFourHourGate() {
        let current = Date(timeIntervalSince1970: 2_000_000)
        let checker = UpdateChecker(now: { current })

        XCTAssertTrue(checker.shouldPerformAutomaticCheck(schedule: .atLaunch, lastCheck: nil))
        XCTAssertTrue(checker.shouldPerformAutomaticCheck(
            schedule: .atLaunch,
            lastCheck: current
        ))
        XCTAssertTrue(checker.shouldPerformAutomaticCheck(
            schedule: .atLaunch,
            lastCheck: current.addingTimeInterval(-1)
        ))
    }

    func testDailyScheduleUsesInjectedClockAndTwentyFourHourBoundary() {
        var current = Date(timeIntervalSince1970: 2_000_000)
        let checker = UpdateChecker(now: { current })

        XCTAssertTrue(checker.shouldPerformAutomaticCheck(schedule: .daily, lastCheck: nil))
        XCTAssertFalse(checker.shouldPerformAutomaticCheck(
            schedule: .daily,
            lastCheck: current.addingTimeInterval(-UpdateChecker.automaticInterval + 1)
        ))

        let previousCheck = current
        current.addTimeInterval(UpdateChecker.automaticInterval)
        XCTAssertTrue(checker.shouldPerformAutomaticCheck(
            schedule: .daily,
            lastCheck: previousCheck
        ))
    }

    func testLatestReleaseRequestHasOnlyTheRequiredAcceptHeader() {
        let request = UpdateChecker().makeLatestReleaseRequest()
        XCTAssertEqual(request.url, UpdateChecker.latestReleaseURL)
        XCTAssertEqual(request.httpMethod, "GET")
        XCTAssertEqual(
            request.value(forHTTPHeaderField: "Accept"),
            "application/vnd.github+json"
        )
        XCTAssertEqual(Set(request.allHTTPHeaderFields?.keys.map { $0 } ?? []), ["Accept"])
        XCTAssertNil(request.value(forHTTPHeaderField: "Authorization"))
    }

    @MainActor
    func testUpdateCheckScheduleDefaultsToDailyAndLastCheckPersists() {
        let suiteName = "castellum-update-test-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suiteName)!
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let prefs = Preferences(defaults: defaults)
        XCTAssertEqual(prefs.updateCheckSchedule, .daily)
        let date = Date(timeIntervalSince1970: 123_456)
        prefs.updateCheckSchedule = .never
        prefs.lastUpdateCheck = date

        let reloaded = Preferences(defaults: defaults)
        XCTAssertEqual(reloaded.updateCheckSchedule, .never)
        XCTAssertEqual(reloaded.lastUpdateCheck, date)
    }

    /// migration: existing true → .daily, false → .never,
    /// unset → .daily. The new enum key, once written, wins over the old one.
    @MainActor
    func testUpdateCheckScheduleMigratesFromTheLegacyBoolPreference() {
        let trueSuite = "castellum-update-migrate-true-\(UUID().uuidString)"
        let trueDefaults = UserDefaults(suiteName: trueSuite)!
        defer { trueDefaults.removePersistentDomain(forName: trueSuite) }
        trueDefaults.set(true, forKey: "checkForUpdatesAutomatically")
        XCTAssertEqual(Preferences(defaults: trueDefaults).updateCheckSchedule, .daily)

        let falseSuite = "castellum-update-migrate-false-\(UUID().uuidString)"
        let falseDefaults = UserDefaults(suiteName: falseSuite)!
        defer { falseDefaults.removePersistentDomain(forName: falseSuite) }
        falseDefaults.set(false, forKey: "checkForUpdatesAutomatically")
        XCTAssertEqual(Preferences(defaults: falseDefaults).updateCheckSchedule, .never)

        let unsetSuite = "castellum-update-migrate-unset-\(UUID().uuidString)"
        let unsetDefaults = UserDefaults(suiteName: unsetSuite)!
        defer { unsetDefaults.removePersistentDomain(forName: unsetSuite) }
        XCTAssertEqual(Preferences(defaults: unsetDefaults).updateCheckSchedule, .daily)
    }

    /// The new enum key wins over a leftover legacy Bool (the reviewer's
    /// untested branch): a user who picked Never must not be flipped back to
    /// Daily by an old `true`.
    @MainActor
    func testUpdateCheckScheduleNewKeyWinsOverLegacyBool() {
        let suite = "castellum-update-migrate-both-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set(true, forKey: "checkForUpdatesAutomatically")
        defaults.set(UpdateCheckSchedule.never.rawValue, forKey: "updateCheckSchedule")
        XCTAssertEqual(Preferences(defaults: defaults).updateCheckSchedule, .never)
    }

    /// Each schedule's Settings caption must describe that schedule: only
    /// Daily may say "once a day", and Never must not claim a check at all.
    func testScheduleDisclosureMatchesEachSchedule() {
        XCTAssertTrue(UpdateCheckSchedule.daily.disclosure.contains("once a day"))
        XCTAssertFalse(UpdateCheckSchedule.atLaunch.disclosure.contains("once a day"))
        XCTAssertTrue(UpdateCheckSchedule.atLaunch.disclosure.contains("starts"))
        XCTAssertFalse(UpdateCheckSchedule.never.disclosure.contains("once a day"))
        XCTAssertTrue(UpdateCheckSchedule.never.disclosure.hasPrefix("Never contacts"))
        for schedule in [UpdateCheckSchedule.atLaunch, .daily] {
            XCTAssertTrue(schedule.disclosure.contains("sends no identifiers"), "\(schedule)")
        }
    }

    // MARK: - Audit #3454: a failed check must not use up the daily slot

    private final class StubProtocol: URLProtocol {
        nonisolated(unsafe) static var status = 200
        nonisolated(unsafe) static var body = Data()
        override class func canInit(with request: URLRequest) -> Bool { true }
        override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
        override func startLoading() {
            let r = HTTPURLResponse(url: request.url!, statusCode: Self.status, httpVersion: nil, headerFields: nil)!
            client?.urlProtocol(self, didReceive: r, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: Self.body)
            client?.urlProtocolDidFinishLoading(self)
        }
        override func stopLoading() {}
    }

    private func stubbedChecker(now: Date) -> UpdateChecker {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [StubProtocol.self]
        return UpdateChecker(session: URLSession(configuration: config), now: { now })
    }

    func testFailedCheckDoesNotRecordTheTimestamp() async {
        StubProtocol.status = 503
        StubProtocol.body = Data()
        var recorded: Date?
        let checker = stubbedChecker(now: Date(timeIntervalSince1970: 1_700_000_000))
        do {
            _ = try await checker.checkRecordingSuccess(currentVersion: "1.0.0") { recorded = $0 }
            XCTFail("a 503 must throw")
        } catch {}
        XCTAssertNil(recorded)
        XCTAssertTrue(checker.shouldPerformAutomaticCheck(schedule: .daily, lastCheck: recorded))
    }

    func testSuccessfulCheckRecordsTheTimestamp() async throws {
        StubProtocol.status = 200
        StubProtocol.body = Data(#"{"tag_name":"v9.9.9"}"#.utf8)
        var recorded: Date?
        let now = Date(timeIntervalSince1970: 1_700_000_000)
        let result = try await stubbedChecker(now: now).checkRecordingSuccess(currentVersion: "1.0.0") { recorded = $0 }
        XCTAssertEqual(result, .updateAvailable(version: "9.9.9"))
        XCTAssertEqual(recorded, now)
    }
}
