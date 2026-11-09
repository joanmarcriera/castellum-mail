// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// GUI-6: click-default policy and Save All planning.
final class AttachmentDefaultActionTests: XCTestCase {
    func testNoPreferenceAsksAndAPreferenceIsPerformed() {
        XCTAssertEqual(AttachmentPrimaryClick.resolve(preference: nil), .ask)
        for action in AttachmentDefaultAction.allCases {
            XCTAssertEqual(AttachmentPrimaryClick.resolve(preference: action), .perform(action))
        }
    }

    func testSaveAllDownloadsOnlyWhatIsNotLocalOrInFlight() {
        let plan = AttachmentBulkPlan.ordinalsToDownload([
            (0, .ready), (1, .idle), (2, .downloading), (3, .failed), (4, .checking),
        ])
        XCTAssertEqual(plan, [1, 3])
    }

    func testBulkProgress() {
        XCTAssertEqual(AttachmentBulkPlan.progress([.ready, .ready]), .allReady)
        XCTAssertEqual(AttachmentBulkPlan.progress([.ready, .downloading]), .waiting)
        XCTAssertEqual(AttachmentBulkPlan.progress([.ready, .idle]), .waiting)
        XCTAssertEqual(AttachmentBulkPlan.progress([.failed, .downloading, .failed]), .failed(count: 2))
    }

    @MainActor
    func testPreferencePersistsAndClears() {
        let suite = "AttachmentDefaultActionTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let prefs = Preferences(defaults: defaults)
        XCTAssertNil(prefs.attachmentDefaultAction)
        prefs.attachmentDefaultAction = .quickLook
        XCTAssertEqual(Preferences(defaults: defaults).attachmentDefaultAction, .quickLook)
        prefs.attachmentDefaultAction = nil
        XCTAssertNil(Preferences(defaults: defaults).attachmentDefaultAction)
    }
}
