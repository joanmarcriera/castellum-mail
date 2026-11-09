// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 Layer A: render real views offscreen at their minimum sizes and
// assert layout invariants (usable text fields, visible footers, no blank
// render, fits a 1280x768 virtual screen). No launch, no network, no real profile.
// Set TEST_RUNNER_CASTELLUM_UI_SNAPSHOT_DIR=<dir> to keep the PNGs + trees
// (`scripts/ui-check.sh --unit` does).
//
// Settings General at its minimum width IS asserted here since T1b (#2646: it used to be
// 514-599pt wide inside a 480pt window). What only the real scene can show (the live
// Settings window being resizable, GUI-3; a SwiftUI-presented sheet's window) stays in
// Layer B: `scripts/ui-check.sh settings-general-clipping gui3-settings-window`.
import AppKit
import SwiftUI
import XCTest
@testable import Castellum

@MainActor
final class UILayoutSnapshotTests: XCTestCase {
    private func prefs() -> Preferences {
        Preferences(defaults: UserDefaults(suiteName: "castellum-uit-\(UUID().uuidString)")!)
    }

    func testAddAccountSheetAsRealSheet() {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        let r = UISnapshotHarness.hostSheet(AccountSetupSheet(model: model), name: "add-account")
        r.assertRules(["nonBlank", "resizable", "movableSheet", "fitsScreen", "noZeroSizeTextFields"])
        XCTAssertTrue(r.nodes.contains { $0.identifier == "sync" }, "Sync footer probe must be discoverable")
        r.parent?.endSheet(r.window)
    }

    func testReauthenticateSheet() {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        let r = UISnapshotHarness.hostSheet(AccountSetupSheet(model: model, existingAccountId: "a@b.test"), name: "reauth")
        r.assertRules(["nonBlank", "resizable", "fitsScreen", "noZeroSizeTextFields"])
        r.parent?.endSheet(r.window)
    }

    func testSettingsTabsRenderAtMinimumSize() {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        let p = prefs()
        for tab in [SettingsTab.general, .ai, .accounts, .tags] {
            model.settingsTab = tab
            let size = NSSize(width: SettingsLayout.minWidth, height: SettingsLayout.minHeight)
            let r = UISnapshotHarness.hostWindow(SettingsView(prefs: p, model: model), size: size, name: "settings-\(tab)")
            // A tab drawn purely by SwiftUI exposes no AppKit nodes; a node rule would prove nothing
            // there (and now says so), so such a tab keeps only the window-level rules.
            r.assertRules(r.nodes.isEmpty ? ["nonBlank", "fitsScreen"] : ["nonBlank", "fitsScreen", "noZeroSizeTextFields"])
        }
    }

    /// #2646 / GUI-3: at the declared minimum width nothing in General may be wider than the
    /// window. The pane scrolls vertically only, so anything wider is clipped for good.
    func testSettingsGeneralFitsItsMinimumWidth() {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        model.settingsTab = .general
        let size = NSSize(width: SettingsLayout.minWidth, height: SettingsLayout.minHeight)
        let r = UISnapshotHarness.hostWindow(SettingsView(prefs: prefs(), model: model), size: size, name: "settings-general-min")
        XCTAssertEqual(r.info.contentRect.width, SettingsLayout.minWidth, accuracy: 1,
                       "the window must actually be at its minimum width for this to prove anything")
        XCTAssertTrue(r.nodes.contains { $0.role == "scrollContent" }, "General must expose its scroll document")
        r.assertRules(["nonBlank", "noClippedText", "controlsReachable"])
    }

    /// The same pane at its minimum width but tall enough to show every row, so the last
    /// control (Default Mail App, the one #2646 clipped) is in the PNG and can be measured.
    func testSettingsGeneralShowsEveryControlAtMinimumWidth() async {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        defer { UIAXDriver.reset() }
        model.settingsTab = .general
        // 1300: the pane grows with each setting (MM-17 added the format=flowed row, MM-14 the Account Setup toggle).
        let size = NSSize(width: SettingsLayout.minWidth, height: 1300)
        let r = UISnapshotHarness.hostWindow(SettingsView(prefs: prefs(), model: model), size: size, name: "settings-general-min-width-full")
        XCTAssertEqual(r.info.contentRect.height, size.height, accuracy: 1,
                       "the window must keep the requested height (no restored autosave frame), or the loop below measures the wrong window")
        r.assertRules(["nonBlank", "noClippedText"])
        // SwiftUI draws bordered buttons and Text itself (no NSView), so the buttons are checked
        // in the PNG; what AppKit backs (radios, checkboxes, pop-ups) is measured here. The rules
        // skip controls inside a scroll view, hence the explicit loop.
        // macOS 27: those are no longer NSControls, and their KeyViewProxy stand-ins are not
        // reliably there in this offscreen window (0 of them on 2026-10-12, on the train base
        // too), so the controls are also read from SwiftUI's accessibility tree (UIAXDriver).
        let content = r.info.contentRect.insetBy(dx: -1, dy: -1)
        await UIAXDriver.ready(r.window)
        let ax = UIAXDriver.entries(in: r.window).map(\.node).filter { ["radio", "checkbox", "popup"].contains($0.role) }
        let controls = r.nodes.filter { !$0.hidden && $0.role != "scrollContent" && $0.cls != "NSScroller" } + ax
        XCTAssertGreaterThanOrEqual(controls.count, 10, "every radio, checkbox and pop-up must be discoverable")
        for c in controls {
            XCTAssertTrue(content.contains(c.frame), "\(c.role) '\(c.label)' \(c.frame) must be fully inside the \(r.info.contentRect.size) window")
        }
    }

    /// FP21: a fresh New Rule sheet shows both switches and Preview Matches without scrolling.
    func testRuleEditorShowsPreviewAndSwitchesWithoutScrolling() {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        let r = UISnapshotHarness.hostSheet(RuleEditorSheet(model: model, initial: RuleDraft()), name: "rule-editor")
        let scroll = r.nodes.first { $0.role == "scrollContent" }
        XCTAssertNotNil(scroll, "the editor must expose its scroll view")
        guard let visible = scroll?.frame else { return }
        let targets = r.nodes.filter { ["stopProcessing", "ruleEnabled", "previewMatches"].contains($0.identifier) }
        XCTAssertGreaterThanOrEqual(targets.count, 3, "two switches and Preview Matches must be discoverable")
        for n in targets {
            XCTAssertGreaterThanOrEqual(n.frame.minY, visible.minY - 1, "\(n.label) is below the fold")
            XCTAssertLessThanOrEqual(n.frame.maxY, visible.maxY + 1, "\(n.label) is above the fold")
        }
        r.parent?.endSheet(r.window)
    }

    /// FP38: the compact Accounts bar (what ViewThatFits falls back to) fits the
    /// minimum Settings width in every shipped language.
    func testAccountsButtonBarFitsMinimumWidth() {
        for code in ["en", "es", "ca"] {
            let view = AccountsActionBar(compact: true, hasSelection: true, onAction: { _ in })
                .environment(\.locale, Locale(identifier: code))
            let hosting = NSHostingView(rootView: view)
            let width = hosting.fittingSize.width
            XCTAssertLessThanOrEqual(width, SettingsLayout.minWidth - 20, "compact bar in \(code) is \(width) pt wide")
        }
    }

    func testSmartMailboxEditorSheet() {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        let r = UISnapshotHarness.hostSheet(SmartMailboxEditorSheet(model: model, existing: nil, prefilledQuery: "from:alice"),
                                            name: "smart-mailbox-editor")
        r.assertRules(["nonBlank", "fitsScreen", "noZeroSizeTextFields"])
        r.parent?.endSheet(r.window)
    }

    /// The harness itself must be able to FAIL: the same sheet on a 400px screen violates fitsScreen.
    func testHarnessCatchesASheetTooBigForASmallScreen() {
        let model = UISnapshotHarness.makeModel()
        defer { try? model.accountStore.deleteAll() }
        let r = UISnapshotHarness.hostSheet(AccountSetupSheet(model: model), name: "add-account-small-screen")
        let tiny = UIVirtualScreen(width: 400, height: 400, menuBar: 25)
        XCTAssertFalse(UIRule.evaluate("fitsScreen", window: r.info, nodes: r.nodes, screen: tiny).isEmpty)
        r.parent?.endSheet(r.window)
    }

    /// The harness can capture a WKWebView body: text is read back and the PNG is not blank.
    func testWebBodyIsCaptured() async {
        let view = EmailBodyWebView(sanitizedHtml: "<h1>Harness body</h1><p>Hello reader pane</p>",
                                    allowRemoteImages: false, onUnrecoverableFailure: {}, onLinkHover: nil)
            .frame(width: 400, height: 200)
        let r = await UISnapshotHarness.hostWindowWithWeb(view, size: NSSize(width: 400, height: 200), name: "web-body")
        XCTAssertTrue(UICapture.lastWebText.contains { $0.contains("Hello reader pane") }, "\(UICapture.lastWebText)")
        XCTAssertGreaterThan(r.info.nonBlankRatio ?? 0, 0.005)
    }
}
