// Castellum - reading-pane webview policy tests (pure functions, no live WKWebView).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import WebKit
import XCTest
@testable import Castellum

final class EmailBodyPolicyTests: XCTestCase {
    // Initial loadHTMLString arrives as .other before any load completed.
    func testInitialLoadIsAllowed() {
        let d = decideBodyNavigation(hasCompletedInitialLoad: false, navigationType: .other, url: nil)
        XCTAssertEqual(d, .allowInitialLoad)
    }

    func testLinkClickOpensExternally() {
        let url = URL(string: "https://example.com/x")!
        let d = decideBodyNavigation(hasCompletedInitialLoad: true, navigationType: .linkActivated, url: url)
        XCTAssertEqual(d, .openExternally(url))
    }

    func testMailtoLinkOpensExternally() {
        let url = URL(string: "mailto:a@b.test")!
        let d = decideBodyNavigation(hasCompletedInitialLoad: true, navigationType: .linkActivated, url: url)
        XCTAssertEqual(d, .openExternally(url))
    }

    func testEverythingElseIsCancelled() {
        // Redirect/meta/js-less form submissions/second .other navigations: cancel.
        XCTAssertEqual(decideBodyNavigation(hasCompletedInitialLoad: true, navigationType: .other, url: URL(string: "https://evil.test")), .cancel)
        XCTAssertEqual(decideBodyNavigation(hasCompletedInitialLoad: true, navigationType: .formSubmitted, url: URL(string: "https://evil.test")), .cancel)
        // A link whose scheme is not http/https/mailto is cancelled, not opened.
        XCTAssertEqual(decideBodyNavigation(hasCompletedInitialLoad: true, navigationType: .linkActivated, url: URL(string: "file:///etc/passwd")), .cancel)
        XCTAssertEqual(decideBodyNavigation(hasCompletedInitialLoad: true, navigationType: .linkActivated, url: nil), .cancel)
    }

    func testHTMLShellEmbedsBodyVerbatimAndOnlyBody() {
        let shell = emailHTMLShell(body: "<p>MARKER</p>")
        XCTAssertTrue(shell.contains("<p>MARKER</p>"))
        XCTAssertTrue(shell.contains("color-scheme"))
        XCTAssertTrue(shell.contains("max-width: 100%"))
        XCTAssertFalse(shell.contains("<script"))
    }

    func testContentRuleListsAreValidJSON() throws {
        for rules in [EmailBodyWebView.blockAllRules, EmailBodyWebView.imagesAllowedRules] {
            let data = Data(rules.utf8)
            let parsed = try JSONSerialization.jsonObject(with: data)
            XCTAssertTrue(parsed is [Any])
        }
    }

    @MainActor
    func testRemoteImagesBlockedByDefault() {
        let suite = UserDefaults(suiteName: "castellum-test-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: suite)
        XCTAssertFalse(prefs.alwaysLoadRemoteImages)
    }

    @MainActor
    func testBackgroundBodyHydrationOffByDefault() {
        // Pilot safety: a fresh profile must NOT auto-download bodies in the
        // background — body-on-open only until the user opts in.
        let suite = UserDefaults(suiteName: "castellum-test-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: suite)
        XCTAssertFalse(prefs.backgroundBodyHydrationEnabled)
        prefs.backgroundBodyHydrationEnabled = true
        XCTAssertTrue(Preferences(defaults: suite).backgroundBodyHydrationEnabled, "persists")
    }
}
