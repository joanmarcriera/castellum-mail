// Castellum - Tests for the compose signature picker and HTML signatures
//: SignatureSwitch, SignatureHTML, the ComposeSession
// wiring, and the core render of an HTML signature under the compose
// sanitiser. Every Preferences here is an isolated UserDefaults suite.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

// MARK: - SignatureSwitch (pure)

final class SignatureSwitchTests: XCTestCase {
    func testNoneToASignatureInsertsIt() {
        let out = SignatureSwitch.apply(from: "", to: "Work", in: "Hello", insertWhenRemoved: true)
        XCTAssertEqual(out, .init(body: "Hello\n\n-- \nWork", applied: true))
    }

    func testAnUnmodifiedSignatureIsSwapped() {
        let out = SignatureSwitch.apply(from: "Work", to: "Home", in: "Hello\n\n-- \nWork", insertWhenRemoved: true)
        XCTAssertEqual(out, .init(body: "Hello\n\n-- \nHome", applied: true))
    }

    func testChoosingNoneRemovesAnUnmodifiedSignature() {
        let out = SignatureSwitch.apply(from: "Work", to: "", in: "Hello\n\n-- \nWork", insertWhenRemoved: true)
        XCTAssertTrue(out.applied)
        XCTAssertFalse(out.body.contains("Work"))
        XCTAssertFalse(out.body.contains("-- "))
    }

    func testAnEditedSignatureIsRefusedAndKept() {
        let body = "Hello\n\n-- \nWork (mobile)"
        let out = SignatureSwitch.apply(from: "Work", to: "Home", in: body, insertWhenRemoved: true)
        XCTAssertEqual(out, .init(body: body, applied: false))
    }

    func testTextAppendedAfterTheSignatureIsRefused() {
        let body = "Hello\n\n-- \nWork\nP.S. call me"
        let out = SignatureSwitch.apply(from: "Work", to: "Home", in: body, insertWhenRemoved: true)
        XCTAssertFalse(out.applied)
        XCTAssertEqual(out.body, body)
    }

    func testEqualTextsCountAsApplied() {
        let body = "Hello\n\n-- \nWork"
        XCTAssertEqual(SignatureSwitch.apply(from: "Work", to: "Work", in: body, insertWhenRemoved: false),
                       .init(body: body, applied: true))
        XCTAssertEqual(SignatureSwitch.apply(from: "", to: "  ", in: "x", insertWhenRemoved: false),
                       .init(body: "x", applied: true))
    }

    func testARemovedSignatureIsReplacedOnlyWhenAsked() {
        let body = "Hello, no signature here"
        XCTAssertEqual(SignatureSwitch.apply(from: "Work", to: "Home", in: body, insertWhenRemoved: true),
                       .init(body: body + "\n\n-- \nHome", applied: true))
        // A From change keeps S26's rule: a signature the user deleted is not re-added.
        XCTAssertEqual(SignatureSwitch.apply(from: "Work", to: "Home", in: body, insertWhenRemoved: false),
                       .init(body: body, applied: false))
    }

    func testAPickedSignatureGoesAboveTheQuote() {
        let body = "\n\nAda wrote:\n> Original"
        let out = SignatureSwitch.apply(from: "", to: "Work", in: body, insertWhenRemoved: true)
        XCTAssertEqual(out.body, "\n\n-- \nWork\n\nAda wrote:\n> Original")
    }

    // MARK: PR #235 review fixes

    /// Review 1: "Ada" is a line-boundary prefix of "Ada\nLovelace"; the
    /// draft carries the longer one, so that is what must be recognised.
    func testRecognisePrefersTheSignatureSpanningTheWholeRegion() {
        let short = SignatureItem(id: "s", name: "Short", accountId: "a", text: "Ada")
        let long = SignatureItem(id: "l", name: "Long", accountId: "a", text: "Ada\nLovelace")
        let body = "Hi\n\n-- \nAda\nLovelace"
        XCTAssertEqual(SignatureSwitch.recognise(in: body, among: [short, long])?.id, "l")
        XCTAssertNil(SignatureSwitch.recognise(in: body, among: [short]), "a prefix is not the signature")
        XCTAssertEqual(SignatureSwitch.recognise(in: "\n\n-- \nAda\n\nBob wrote:\n> q", among: [short, long])?.id, "s")
    }

    /// Review 3: a pasted/forwarded copy of the block higher up is not the
    /// signature; only the last one, starting its line, is.
    func testBlockRangeIsTheLastWholeRegionStartingALine() throws {
        let body = "Fwd:\n-- \nAda\n\nMy text\n\n-- \nAda"
        let r = try XCTUnwrap(SignatureSwitch.blockRange(of: "Ada", in: body))
        XCTAssertEqual(String(body[r.lowerBound...]), "-- \nAda")
        let out = SignatureHTML.markdownSource(body: body, plain: "Ada", html: "<b>Ada</b>")
        XCTAssertTrue(out.hasPrefix("Fwd:\n-- \nAda\n\nMy text\n\n-- \n\n<div>"), out)
        XCTAssertNil(SignatureSwitch.blockRange(of: "Ada", in: "x -- \nAda"), "must start a line")
        XCTAssertNil(SignatureSwitch.blockRange(of: "Ada", in: "-- \nAda\nP.S. more"), "text right below is not a gap")
    }

    /// Review 4: "\r\n" is one Character in Swift, so splitting on "\n" saw
    /// a CRLF body as a single line.
    func testDelimiterDetectionHandlesCRLF() {
        XCTAssertTrue(SignatureSwitch.hasDelimiterLine("Hi\r\n-- \r\nAda"))
        XCTAssertTrue(SignatureSwitch.hasDelimiterLine("Hi\r\n\u{2014}\r\nAda"))
        XCTAssertEqual(SignatureHTML.markdownBlock("<p>a</p>\r\n\r\n<p>b</p>"), "<div>\n<p>a</p>\n<p>b</p>\n</div>\n")
    }

    func testDelimiterLineDetection() {
        XCTAssertTrue(SignatureSwitch.hasDelimiterLine("a\n-- \nb"))
        XCTAssertTrue(SignatureSwitch.hasDelimiterLine("a\n--\nb"))
        XCTAssertTrue(SignatureSwitch.hasDelimiterLine("a\n\u{2014} \nb"))
        XCTAssertFalse(SignatureSwitch.hasDelimiterLine("a -- b\n> -- \n"))
    }

    func testRecogniseFindsTheSignatureADraftCarries() {
        let items = [SignatureItem(id: "w", name: "Work", accountId: "a", text: "Work"),
                     SignatureItem(id: "h", name: "Home", accountId: "a", text: "Home")]
        XCTAssertEqual(SignatureSwitch.recognise(in: "x\n\n-- \nHome", among: items)?.id, "h")
        XCTAssertNil(SignatureSwitch.recognise(in: "x\n\n-- \nHome (edited)\n", among: items.filter { $0.id == "w" }))
        // "-- \nAda" is a prefix of "-- \nAda, Work": the block must end its line.
        let ada = [SignatureItem(id: "a", name: "Short", accountId: "a", text: "Ada")]
        XCTAssertNil(SignatureSwitch.recognise(in: "x\n\n-- \nAda, Work", among: ada))
        XCTAssertEqual(SignatureSwitch.recognise(in: "-- \nAda, Work\n\n-- \nAda\n", among: ada)?.id, "a")
    }
}

// MARK: - SignatureHTML (pure)

final class SignatureHTMLTests: XCTestCase {
    func testPlainTextFromHTML() {
        let html = """
        <p><b>Ada Lovelace</b><br>Analytical Engines &amp; Co.</p>
        <p><a href="https://ada.example">ada.example</a> &nbsp;&#8226;&#x20AC;</p>
        <script>alert(1)</script><style>p{}</style>
        """
        XCTAssertEqual(SignatureHTML.plainText(from: html),
                       "Ada Lovelace\nAnalytical Engines & Co.\n\nada.example •€")
    }

    func testPlainTextDropsCommentsAndUnterminatedScripts() {
        XCTAssertEqual(SignatureHTML.plainText(from: "Ada<!-- a > b secret -->\n<b>L</b><script>x"), "Ada L")
        XCTAssertEqual(SignatureHTML.plainText(from: "Ada <!-- never closed"), "Ada")
    }

    /// Review 6: no NUL or other control character reaches the text part.
    func testPlainTextDropsControlCharacters() {
        XCTAssertEqual(SignatureHTML.plainText(from: "Ada&#0;&#7; L\u{0}x&#x1B;"), "Ada Lx")
        XCTAssertEqual(SignatureHTML.plainText(from: "a&#9;b"), "a b", "an encoded tab is whitespace")
        XCTAssertFalse(SignatureHTML.plainText(from: "&#0;").unicodeScalars.contains { $0.value == 0 })
    }

    func testPlainTextKeepsEscapedMarkupLiteral() {
        XCTAssertEqual(SignatureHTML.plainText(from: "a &amp;lt;b&amp;gt;"), "a &lt;b&gt;")
    }

    func testBoldAndItalicMapToTheTagsTheSanitiserKeeps() {
        XCTAssertEqual(SignatureHTML.normalised("<B class=x>Ada</B> <i>L</i> <br><blockquote>q</blockquote>"),
                       "<strong>Ada</strong> <em>L</em> <br><blockquote>q</blockquote>")
    }

    func testMarkdownBlockDropsBlankLines() {
        XCTAssertEqual(SignatureHTML.markdownBlock("<p>a</p>\n\n   \n    <p>b</p>"),
                       "<div>\n<p>a</p>\n    <p>b</p>\n</div>\n")
    }

    func testMarkdownSourceReplacesAnUnmodifiedBlockOnly() {
        let body = "Hi\n\n-- \nAda\n\nBob wrote:\n> q"
        let out = SignatureHTML.markdownSource(body: body, plain: "Ada", html: "<b>Ada</b>")
        XCTAssertEqual(out, "Hi\n\n-- \n\n<div>\n<strong>Ada</strong>\n</div>\n\n\nBob wrote:\n> q")
        // Edited on the same line, or no HTML: the body renders as typed.
        XCTAssertEqual(SignatureHTML.markdownSource(body: "Hi\n\n-- \nAda L.", plain: "Ada", html: "<b>Ada</b>"),
                       "Hi\n\n-- \nAda L.")
        XCTAssertEqual(SignatureHTML.markdownSource(body: body, plain: "Ada", html: "  "), body)
        XCTAssertEqual(SignatureHTML.markdownSource(body: body, plain: "Ada", html: nil), body)
    }
}

// MARK: - ComposeSession wiring

@MainActor
final class ComposeSignaturePickerTests: XCTestCase {
    private func prefs() -> Preferences {
        Preferences(defaults: UserDefaults(suiteName: "signature-picker-\(UUID().uuidString)")!)
    }

    /// acctA: Work (default) + Home; acctB: Grace (default).
    private func library(_ p: Preferences) -> (work: String, home: String, grace: String) {
        let work = p.signatureLibrary.add(name: "Work", accountId: "acctA", text: "Ada, Work")
        let home = p.signatureLibrary.add(name: "Home", accountId: "acctA", text: "Ada")
        let grace = p.signatureLibrary.add(name: "Grace", accountId: "acctB", text: "Grace Hopper")
        return (work, home, grace)
    }

    private func newSession(_ p: Preferences, account: String = "acctA") -> ComposeSession {
        ComposeSession(accountId: account, signature: p.signature(for: account), preferences: p)
    }

    func testANewMessageStartsOnTheAccountDefault() {
        let p = prefs()
        let ids = library(p)
        let s = newSession(p)
        XCTAssertEqual(s.signatureId, ids.work)
        XCTAssertEqual(s.body, "\n\n-- \nAda, Work")
    }

    func testPickingAnotherSignatureSwapsItAndCountsAsAnEdit() {
        let p = prefs()
        let ids = library(p)
        let s = newSession(p)
        s.chooseSignature(ids.home)
        XCTAssertEqual(s.signatureId, ids.home)
        XCTAssertEqual(s.body, "\n\n-- \nAda")
        XCTAssertNil(s.signatureNotice)
        XCTAssertTrue(s.hasUserEdits)
    }

    func testPickingNoneRemovesItAndPickingAgainReinserts() {
        let p = prefs()
        let ids = library(p)
        let s = newSession(p)
        s.body = "Dear Bob," + s.body
        s.chooseSignature(nil)
        XCTAssertNil(s.signatureId)
        XCTAssertFalse(s.body.contains("Work"))
        s.chooseSignature(ids.home)
        XCTAssertEqual(s.signatureId, ids.home)
        XCTAssertTrue(s.body.hasPrefix("Dear Bob,"))
        XCTAssertTrue(s.body.hasSuffix("-- \nAda"))
    }

    func testAnEditedSignatureIsKeptAndTheNoticeExplains() {
        let p = prefs()
        let ids = library(p)
        let s = newSession(p)
        s.body = s.body.replacingOccurrences(of: "Ada, Work", with: "Ada, Work (mobile)")
        let before = s.body
        s.chooseSignature(ids.home)
        XCTAssertEqual(s.body, before)
        XCTAssertEqual(s.signatureId, ids.work, "the picker stays on what is in the message")
        XCTAssertNotNil(s.signatureNotice)
    }

    func testSwapsUseTheTextAsInsertedNotTheLibrarysCurrentText() {
        let p = prefs()
        let ids = library(p)
        let s = newSession(p)
        p.signatureLibrary.setText(id: ids.work, "Ada, Work (new title)")   // edited in Settings meanwhile
        s.chooseSignature(ids.home)
        XCTAssertEqual(s.body, "\n\n-- \nAda")
    }

    func testFromChangeMovesThePickerToTheNewAccountsDefault() {
        let p = prefs()
        let ids = library(p)
        let s = newSession(p)
        s.chooseSignature(ids.home)
        s.accountId = "acctB"
        XCTAssertEqual(s.signatureId, ids.grace)
        XCTAssertEqual(s.body, "\n\n-- \nGrace Hopper")
    }

    func testFromChangeWithAnEditedSignatureClearsThePickerAndKeepsTheText() {
        let p = prefs()
        _ = library(p)
        let s = newSession(p)
        s.body = s.body + " (mobile)"
        s.accountId = "acctB"
        XCTAssertNil(s.signatureId)
        XCTAssertTrue(s.body.contains("Ada, Work (mobile)"))
        XCTAssertFalse(s.body.contains("Grace"))
    }

    func testAReopenedDraftRecognisesItsSignature() {
        let p = prefs()
        let ids = library(p)
        let draft = Draft(id: 4, accountId: "acctA", to: [], cc: [], bcc: [], subject: "S",
                          bodyText: "Hi\n\n-- \nAda", bodyHtml: nil, fromEmail: nil, fromName: nil,
                          inReplyTo: nil, referencesHeader: nil, status: "draft", updatedAtEpoch: 0,
                          serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
                          serverMessageId: nil, serverSavedAtEpoch: nil)
        let s = ComposeSession(draft: draft, preferences: p)
        XCTAssertEqual(s.signatureId, ids.home)
        XCTAssertEqual(s.body, "Hi\n\n-- \nAda", "reopening never inserts")
        s.chooseSignature(ids.work)
        XCTAssertEqual(s.body, "Hi\n\n-- \nAda, Work")
    }

    func testADraftWithAnEditedSignatureNeverGetsASecondOne() {
        let p = prefs()
        let ids = library(p)
        let draft = Draft(id: 5, accountId: "acctA", to: [], cc: [], bcc: [], subject: "S",
                          bodyText: "Hi\n\n-- \nAda, Work (mobile)", bodyHtml: nil, fromEmail: nil, fromName: nil,
                          inReplyTo: nil, referencesHeader: nil, status: "draft", updatedAtEpoch: 0,
                          serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
                          serverMessageId: nil, serverSavedAtEpoch: nil)
        let s = ComposeSession(draft: draft, preferences: p)
        XCTAssertNil(s.signatureId)
        s.chooseSignature(ids.home)
        XCTAssertEqual(s.body, draft.bodyText)
        XCTAssertNotNil(s.signatureNotice)
        s.accountId = "acctB"
        XCTAssertEqual(s.body, draft.bodyText, "a From change does not stack another signature either")
    }

    /// Security review (stale identity state): after a refused From change the
    /// previous account's HTML signature must not be substituted into the
    /// HTML part, even though its plain block is still in the body.
    func testARefusedFromChangeDropsThePreviousAccountsHTML() {
        let p = prefs()
        let ids = library(p)
        p.signatureLibrary.setHTML(id: ids.work, "<p><a href=\"https://work.example/secret\">Work</a></p>")
        let s = newSession(p)
        XCTAssertTrue(s.markdownSourceTransform()(s.body).contains("work.example/secret"))
        s.body += "\nP.S. sent from my phone"   // appended: the swap is refused
        s.accountId = "acctB"
        XCTAssertTrue(s.body.contains("-- \nAda, Work\nP.S."), "the user's text stays")
        XCTAssertNil(s.insertedSignatureHTML)
        XCTAssertFalse(s.markdownSourceTransform()(s.body).contains("work.example"))
    }

    func testAnAppliedFromChangeCarriesOnlyTheNewAccountsHTML() {
        let p = prefs()
        let ids = library(p)
        p.signatureLibrary.setHTML(id: ids.work, "<b>Work HTML</b>")
        p.signatureLibrary.setHTML(id: ids.grace, "<b>Grace HTML</b>")
        let s = newSession(p)
        s.accountId = "acctB"
        let source = s.markdownSourceTransform()(s.body)
        XCTAssertTrue(source.contains("Grace HTML"))
        XCTAssertFalse(source.contains("Work"))
        XCTAssertFalse(s.body.contains("Ada"))
    }

    /// Review 2 (product call): an explicit "No signature" is honoured across
    /// a From change; picking a signature again lifts it.
    func testAnExplicitNoSignatureSurvivesAFromChange() {
        let p = prefs()
        let ids = library(p)
        let s = newSession(p)
        s.body = "Dear Bob," + s.body
        s.chooseSignature(nil)
        s.accountId = "acctB"
        XCTAssertNil(s.signatureId)
        XCTAssertFalse(s.body.contains("Grace"), s.body)
        XCTAssertFalse(s.body.contains("-- "), s.body)
        s.accountId = "acctA"
        XCTAssertFalse(s.body.contains("-- "), "still unsigned: \(s.body)")
        s.chooseSignature(ids.home)
        s.accountId = "acctB"
        XCTAssertEqual(s.signatureId, ids.grace, "a later pick lifts the choice")
        XCTAssertTrue(s.body.hasSuffix("-- \nGrace Hopper"), s.body)
    }

    /// Review 2 + #3142 review 1 together: a reopened draft carrying the
    /// longer of two prefix-sharing signatures recognises it, so the next
    /// pick swaps instead of being refused with an "edited" caption.
    func testADraftWithTheLongerOfTwoPrefixSignaturesSwapsCleanly() {
        let p = prefs()
        let short = p.signatureLibrary.add(name: "Short", accountId: "acctD", text: "Ada")
        let long = p.signatureLibrary.add(name: "Long", accountId: "acctD", text: "Ada\nLovelace")
        let draft = Draft(id: 6, accountId: "acctD", to: [], cc: [], bcc: [], subject: "S",
                          bodyText: "Hi\n\n-- \nAda\nLovelace", bodyHtml: nil, fromEmail: nil, fromName: nil,
                          inReplyTo: nil, referencesHeader: nil, status: "draft", updatedAtEpoch: 0,
                          serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
                          serverMessageId: nil, serverSavedAtEpoch: nil)
        let s = ComposeSession(draft: draft, preferences: p)
        XCTAssertEqual(s.signatureId, long)
        s.chooseSignature(short)
        XCTAssertNil(s.signatureNotice)
        XCTAssertEqual(s.body, "Hi\n\n-- \nAda")
    }

    /// Review 5: when compose inserted nothing and the "-- " line was typed
    /// by hand, the caption must not claim a signature was "edited". Only
    /// reachable once PR #225 refuses to stack under an existing delimiter.
    func testAHandTypedDelimiterGetsAnAccurateCaption() throws {
        let p = prefs()
        let ids = library(p)
        let s = ComposeSession(accountId: "acctA", signature: "", preferences: p)
        s.body = "Hi\n\n-- \nMe, typed"
        s.chooseSignature(ids.home)
        try XCTSkipIf(s.body != "Hi\n\n-- \nMe, typed", "needs PR #225's delimiter detection (stacks on main)")
        let notice = try XCTUnwrap(s.signatureNotice)
        XCTAssertFalse(notice.contains("edited"), notice)
        XCTAssertTrue(notice.contains("already has"), notice)
    }

    func testAnHTMLOnlySignatureInsertsItsTextAndFeedsTheHTMLPart() {
        let p = prefs()
        let id = p.signatureLibrary.add(name: "Rich", accountId: "acctC", text: "")
        p.signatureLibrary.setHTML(id: id, "<p><b>Ada</b><br>Engines</p>")
        let s = newSession(p, account: "acctC")
        XCTAssertEqual(s.signatureId, id)
        XCTAssertEqual(s.body, "\n\n-- \nAda\nEngines")
        let source = s.markdownSourceTransform()(s.body)
        XCTAssertTrue(source.contains("<strong>Ada</strong>"))
        XCTAssertFalse(source.contains("-- \nAda\nEngines"))
    }
}

// MARK: - the core render (compose sanitiser) of an HTML signature

@MainActor
final class SignatureHTMLRenderTests: XCTestCase {
    func testHTMLSignatureIsRenderedThroughTheComposeSanitiser() throws {
        let core = try CastellumCore(profilePath: TestScratch.root
            .appendingPathComponent("castellum-signature-html-\(UUID().uuidString)"))
        let html = """
        <p><b>Ada Lovelace</b><br><a href="https://ada.example">site</a></p>

        <img src="https://tracker.example/p.gif"><span style="color:red">red</span>
        <a href="javascript:alert(1)">bad</a><script>alert(2)</script>
        """
        let body = "Thanks\n\n-- \nAda Lovelace\n\nBob wrote:\n> quoted"
        let source = SignatureHTML.markdownSource(body: body, plain: "Ada Lovelace", html: html)
        let out = core.renderMarkdown(source, theme: .plain)
        XCTAssertTrue(out.contains("<strong>Ada Lovelace</strong>"), out)
        XCTAssertTrue(out.contains("href=\"https://ada.example\""), out)
        XCTAssertTrue(out.contains("red"), "a disallowed tag is unwrapped to its text")
        XCTAssertTrue(out.contains("<blockquote>"), "the quote after the signature still renders as Markdown: \(out)")
        // `span` is in the compose allowlist since fenced-code highlighting
        // (#3148), but only with the highlighter's fixed `style` values: a
        // signature's own span survives bare, with every attribute dropped.
        for banned in ["<img", "style=", "javascript:", "<script", "alert(2)", "<div", "<span "] {
            XCTAssertFalse(out.contains(banned), "\(banned) survived: \(out)")
        }
    }

    /// Security review (parser differential / hidden content): the sent HTML
    /// part holds only the sanitised signature, with nothing a recipient's
    /// client could show or hide differently from the preview: no comments,
    /// no style (so no display:none), no hidden attribute, and the plain-text
    /// version derived from it carries the same visible text and no comment.
    func testHiddenContentNeverReachesEitherPart() throws {
        let core = try CastellumCore(profilePath: TestScratch.root
            .appendingPathComponent("castellum-signature-hidden-\(UUID().uuidString)"))
        let html = """
        <p>Ada<!-- internal: salary 90k --></p>
        <p style="display:none">tracking</p><span hidden>quiet</span>
        <svg><text>vector</text></svg><iframe src="https://x.example"></iframe>
        """
        let plain = SignatureHTML.plainText(from: html)
        XCTAssertFalse(plain.contains("salary"), plain)
        let source = SignatureHTML.markdownSource(body: "Hi\n\n-- \n\(plain)", plain: plain, html: html)
        let sent = core.renderMarkdown(source, theme: .plain)
        for banned in ["<!--", "salary", "style=", "display", "hidden", "<svg", "<iframe", "x.example"] {
            XCTAssertFalse(sent.contains(banned), "\(banned) survived: \(sent)")
        }
        // Text the sanitiser keeps visible is in both parts.
        for visible in ["Ada", "tracking", "quiet"] {
            XCTAssertTrue(sent.contains(visible), sent)
            XCTAssertTrue(plain.contains(visible), plain)
        }
    }

    /// The editor preview and the sent part are the same render of the same
    /// sanitised HTML: the preview fragment appears verbatim in the message.
    func testPreviewMatchesTheSentSignature() throws {
        let core = try CastellumCore(profilePath: TestScratch.root
            .appendingPathComponent("castellum-signature-same-\(UUID().uuidString)"))
        let html = "<p><b>Ada</b> &amp; <i>Co</i><br><a href=\"mailto:ada@x.test\">mail</a></p>"
        let preview = core.renderMarkdownPreview(SignatureHTML.previewSource(html))
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let source = SignatureHTML.markdownSource(body: "Thanks\n\n-- \nAda & Co", plain: "Ada & Co", html: html)
        let sent = core.renderMarkdown(source, theme: .plain)
        XCTAssertFalse(preview.isEmpty)
        XCTAssertTrue(sent.contains(preview), "preview:\n\(preview)\nsent:\n\(sent)")
    }

    func testEditorPreviewUsesTheSameSanitiser() throws {
        let core = try CastellumCore(profilePath: TestScratch.root
            .appendingPathComponent("castellum-signature-preview-\(UUID().uuidString)"))
        let out = core.renderMarkdownPreview(SignatureHTML.previewSource("<i>x</i><img src=y>"))
        XCTAssertTrue(out.contains("<em>x</em>"), out)
        XCTAssertFalse(out.contains("<img"), out)
    }
}
