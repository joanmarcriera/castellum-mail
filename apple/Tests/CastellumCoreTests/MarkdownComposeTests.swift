// Castellum - Tests for S46's ComposeSession Markdown state and the
// renderMarkdown injection into buildDraftForSend/buildDraftForSave.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class MarkdownComposeTests: XCTestCase {
    private func stored(bodyHtml: String?) -> Draft {
        Draft(id: 9, accountId: "acct", to: [DraftAddress(name: nil, email: "a@x.test")],
              cc: [], bcc: [], subject: "S", bodyText: "**md**", bodyHtml: bodyHtml,
              fromEmail: nil, fromName: nil,
              inReplyTo: nil, referencesHeader: nil, status: "draft", updatedAtEpoch: 0,
              serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
              serverMessageId: nil, serverSavedAtEpoch: nil)
    }

    func testFreshSessionStartsFromTheAccountsMarkdownDefault() {
        let defaults = UserDefaults(suiteName: "markdown-compose-tests-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        prefs.setMarkdownDefault(true, for: "acct-on")
        let onSession = ComposeSession(accountId: "acct-on", preferences: prefs)
        XCTAssertTrue(onSession.markdownEnabled)
        let offSession = ComposeSession(accountId: "acct-off", preferences: prefs)
        XCTAssertFalse(offSession.markdownEnabled, "no entry -> plain text, matching pre-S46 profiles")
    }

    func testReopeningADraftDerivesMarkdownEnabledFromStoredBodyHtmlNotTheAccountDefault() {
        let defaults = UserDefaults(suiteName: "markdown-compose-tests-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        prefs.setMarkdownDefault(true, for: "acct") // default ON…
        let plainDraft = ComposeSession(draft: stored(bodyHtml: nil), preferences: prefs)
        XCTAssertFalse(plainDraft.markdownEnabled, "…but THIS draft was saved plain, so it reopens plain")
        let mdDraft = ComposeSession(draft: stored(bodyHtml: "<p><strong>md</strong></p>"), preferences: prefs)
        XCTAssertTrue(mdDraft.markdownEnabled)
    }

    func testBuildDraftForSendNeverCallsRenderMarkdownWhenDisabled() {
        let s = ComposeSession(accountId: "acct")
        s.toText = "b@x.test"
        s.markdownEnabled = false
        var called = false
        let (draft, _) = s.buildDraftForSend(renderMarkdown: { _ in called = true; return "should not run" })
        XCTAssertFalse(called)
        XCTAssertNil(draft?.bodyHtml)
    }

    func testBuildDraftForSendRendersAndStoresHtmlWhenEnabled() {
        let s = ComposeSession(accountId: "acct")
        s.toText = "b@x.test"
        s.markdownEnabled = true
        s.body = "**hi**"
        let (draft, _) = s.buildDraftForSend(renderMarkdown: { md in "<p><strong>\(md)</strong></p>" })
        XCTAssertEqual(draft?.bodyHtml, "<p><strong>**hi**</strong></p>")
        XCTAssertEqual(draft?.bodyText, "**hi**", "the SOURCE, not the render, is bodyText")
    }

    func testBuildDraftForSaveAlsoHonoursMarkdownEnabled() {
        let s = ComposeSession(accountId: "acct")
        s.markdownEnabled = true
        s.body = "text"
        let draft = s.buildDraftForSave(renderMarkdown: { _ in "<p>text</p>" })
        XCTAssertEqual(draft.bodyHtml, "<p>text</p>")
    }

    // ---- MM-5 themes ----

    private func core() throws -> CastellumCore {
        try CastellumCore(profilePath: TestScratch.root
            .appendingPathComponent("castellum-swift-md-theme-\(UUID().uuidString)"))
    }

    func testEveryThemeRendersThroughTheRealCoreAndPlainIsUnstyled() throws {
        let core = try core()
        let md = "# Title\n\n> quoted\n\n```\ncode\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |"
        let plain = core.renderMarkdown(md, theme: .plain)
        XCTAssertEqual(plain, core.renderMarkdownPreview(md), "plain is the S46 output")
        XCTAssertFalse(plain.contains("style="))
        for theme in MarkdownThemeChoice.allCases where theme != .plain {
            let html = core.renderMarkdown(md, theme: theme)
            XCTAssertTrue(html.hasPrefix("<div style="), "\(theme)")
            XCTAssertTrue(html.contains("<blockquote style=") && html.contains("<pre style=") && html.contains("<th style="), "\(theme)")
            XCTAssertFalse(html.contains("<script") || html.contains("<img"), "\(theme)")
        }
    }

    func testSessionUsesThePreferenceThemeAndCapturesItAtBuildTime() throws {
        let defaults = UserDefaults(suiteName: "markdown-theme-tests-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        XCTAssertEqual(prefs.markdownTheme, .plain, "existing users keep what they sent before")
        prefs.markdownTheme = .serif
        prefs.setMarkdownDefault(true, for: "acct")
        let session = ComposeSession(accountId: "acct", preferences: prefs)
        XCTAssertEqual(session.markdownTheme, .serif)
        XCTAssertEqual(Preferences(defaults: defaults).markdownTheme, .serif, "persisted")
        let core = try core()
        session.body = "**hi**"
        session.toText = "a@x.test"
        let render = session.markdownRenderer(core: core)
        session.markdownTheme = .compact    // a later change must not alter this renderer
        let html = render("**hi**")
        XCTAssertTrue(html?.contains("Georgia") == true, "captured the serif theme, got \(html ?? "nil")")
        XCTAssertTrue(session.markdownRenderer(core: core)("**hi**")?.contains("Georgia") == false)
    }

    func testUnknownStoredThemeFallsBackToPlain() {
        let defaults = UserDefaults(suiteName: "markdown-theme-tests-\(UUID().uuidString)")!
        defaults.set("neon", forKey: "markdownTheme")
        XCTAssertEqual(Preferences(defaults: defaults).markdownTheme, .plain)
    }

    func testReopenedDraftKeepsItsThemeInsteadOfTheSettingsDefault() throws {
        let core = try core()
        for theme in MarkdownThemeChoice.allCases {
            let html = core.renderMarkdown("# T\n\ntext", theme: theme)
            XCTAssertEqual(MarkdownThemeChoice.infer(fromHtml: html), theme, "\(theme)")
        }
        XCTAssertNil(MarkdownThemeChoice.infer(fromHtml: nil))
        let defaults = UserDefaults(suiteName: "markdown-theme-tests-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        prefs.markdownTheme = .plain
        let serifHtml = core.renderMarkdown("**md**", theme: .serif)
        let session = ComposeSession(draft: stored(bodyHtml: serifHtml), preferences: prefs)
        XCTAssertEqual(session.markdownTheme, .serif, "the draft's own theme wins over the Settings default")
    }
}
