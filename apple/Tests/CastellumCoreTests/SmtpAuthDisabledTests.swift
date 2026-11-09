// Castellum - Microsoft "SMTP AUTH is switched off" send refusal (MS1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A real Outlook.com account had every send refused on 2026-10-05 with the
// reply in `rawReason` below. It is a mailbox or tenant setting, not a
// credential problem, so the app must not promise an automatic retry, and
// must name the setting and link to Microsoft's page about it. (The S37
// OAuth-refresh half of this suite stays on the Microsoft OAuth branch.)
//
// Pure presentation tests: nothing here touches a server, Keychain or profile.

import XCTest
@testable import Castellum

@MainActor
final class SmtpAuthDisabledTests: XCTestCase {
    /// `pending_operations.last_error` exactly as the core records it for the
    /// reply the real account got: the stable `<kind>: ` token, then lettre's
    /// rendering of the server's own text.
    private let rawReason = "auth_smtp_disabled: permanent error (535): 5.7.139 Authentication unsuccessful, "
        + "SmtpClientAuthentication is disabled for the Mailbox. Visit https://aka.ms/smtp_auth_disabled"
    /// An ordinary rejected credential, for contrast.
    private let rawAuthReason = "auth: permanent error (535): 5.7.3 Authentication unsuccessful"

    // MARK: - the guidance is what the person sees

    func testTheKindIsRecognisedFromTheRecordedReason() {
        XCTAssertTrue(SmtpAuthDisabledGuidance.applies(to: rawReason))
        XCTAssertFalse(SmtpAuthDisabledGuidance.applies(to: rawAuthReason))
        XCTAssertFalse(SmtpAuthDisabledGuidance.applies(to: "Authentication unsuccessful 5.7.139"))
        XCTAssertFalse(SmtpAuthDisabledGuidance.applies(to: nil))
    }

    func testTheComposeCaptionNamesTheSettingAndPromisesNoAutomaticRetry() {
        let caption = SendFailurePresentation.outboxMessage(
            rawReason: rawReason, endpoint: "smtp.office365.com:587")

        XCTAssertTrue(caption.contains(SmtpAuthDisabledGuidance.headline), caption)
        XCTAssertTrue(caption.contains("Authenticated SMTP"), caption)
        XCTAssertFalse(caption.localizedCaseInsensitiveContains("retry automatically"), caption)
        XCTAssertFalse(caption.localizedCaseInsensitiveContains("username or password"), caption)
        // The server's own text never reaches the caption.
        XCTAssertFalse(caption.contains("5.7.139"), caption)
    }

    func testTheGuidanceNamesTheSettingForBothKindsOfMailboxAndLinksToMicrosoft() {
        let detail = SmtpAuthDisabledGuidance.detail
        XCTAssertTrue(detail.contains("Authenticated SMTP"))
        XCTAssertTrue(detail.contains("Microsoft 365 admin centre"))
        XCTAssertTrue(detail.contains("Outlook.com"))
        XCTAssertTrue(detail.contains("Retry"))
        XCTAssertEqual(SmtpAuthDisabledGuidance.helpURL.absoluteString, "https://aka.ms/smtp_auth_disabled")
    }

    /// What `ComposeView` does with a `.leftInOutbox` outcome: the session
    /// carries the caption and the flag that makes the view add the link.
    func testAComposeSessionLeftInTheOutboxForThisKindShowsTheGuidanceLink() {
        var presentation = SendFailurePresentation.composePresentation(
            rawReason: rawReason, endpoint: "smtp.office365.com:587")
        XCTAssertTrue(presentation.showsSmtpAuthDisabledHelp)
        XCTAssertTrue(presentation.caption.contains(SmtpAuthDisabledGuidance.headline))

        presentation = SendFailurePresentation.composePresentation(
            rawReason: rawAuthReason, endpoint: "smtp.office365.com:587")
        XCTAssertFalse(presentation.showsSmtpAuthDisabledHelp)
    }

    func testTheOutboxRowSaysWhyItStoppedInsteadOfCountingAttempts() {
        let (label, needsAttention) = outboxStatusLabel(
            status: "failed_permanent", attempts: 1, lastError: rawReason)
        XCTAssertTrue(needsAttention)
        XCTAssertEqual(label, "Needs attention — \(SmtpAuthDisabledGuidance.headline)")

        // Any other permanent failure keeps its wording.
        XCTAssertEqual(
            outboxStatusLabel(status: "failed_permanent", attempts: 8, lastError: "network: down").0,
            "Needs attention — gave up after 8 attempts")
        XCTAssertTrue(outboxCanRetry(status: "failed_permanent"), "the manual Retry stays available")
    }

    /// The background-drain notice must not call it a rejected sign-in
    /// (its kind token contains "auth", which the notice's word match keys on).
    func testTheFailedSendNoticeNamesTheSettingNotASignInFailure() {
        let notice = SendFailureNotice.message(failed: 1, thrown: nil, lastError: rawReason)
        XCTAssertEqual(notice, "Not sent — kept in Outbox. \(SmtpAuthDisabledGuidance.headline)")
        XCTAssertEqual(SendFailureNotice.reason(for: rawAuthReason),
                       "The mail server refused the sign-in.")
    }

    func testTheConnectionTestExplainsTheSettingBeforeTheFirstSend() {
        let line = ConnectionTestPresentation.line(
            for: ConnectionLegResult(ok: false, errorKind: "auth_smtp_disabled",
                                     summary: "SMTP sending is switched off for this mailbox"),
            protocolName: "SMTP", endpoint: "smtp.office365.com:587", successText: "unused")
        XCTAssertFalse(line.ok)
        XCTAssertTrue(line.text.contains(SmtpAuthDisabledGuidance.headline), line.text)
        XCTAssertTrue(line.text.contains("Authenticated SMTP"), line.text)
        XCTAssertFalse(line.text.contains("check the username and password"), line.text)
    }

    // MARK: - translations

    /// The MS1 guidance exists in English, Spanish and Catalan, and the
    /// translations are not the English text.
    func testGuidanceStringsExistInEnglishSpanishAndCatalan() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/Castellum/Resources/Localizable.xcstrings")
        let root = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        let strings = root["strings"] as! [String: [String: Any]]
        let keys = [
            // MS1 guidance.
            "Microsoft has switched off SMTP sending for this mailbox.",
            "Signing in again will not help. The setting is called Authenticated SMTP (SMTP AUTH). For a work or school mailbox, ask your administrator to turn on Authenticated SMTP for it in the Microsoft 365 admin centre (Users > Active users > Mail > Manage email apps). A personal Outlook.com mailbox has no such switch, so Castellum cannot send from it yet.",
            "Your message stays in the Outbox; choose Retry there once the setting has changed.",
            "Open Microsoft’s SMTP AUTH help",
            "Not sent — kept in Outbox. %@",
            "Needs attention — %@",
        ]
        for key in keys {
            let locs = try XCTUnwrap(strings[key]?["localizations"] as? [String: [String: Any]], "missing: \(key)")
            XCTAssertEqual(Set(locs.keys), ["en", "es", "ca"], key)
            let value: (String) -> String? = { lang in
                (locs[lang]?["stringUnit"] as? [String: Any])?["value"] as? String
            }
            XCTAssertEqual(value("en"), key)
            for lang in ["es", "ca"] {
                let translated = try XCTUnwrap(value(lang), "\(lang): \(key)")
                XCTAssertFalse(translated.isEmpty, "\(lang): \(key)")
                XCTAssertNotEqual(translated, key, "\(lang) is still the English text: \(key)")
            }
        }
        // The guidance resolves through the catalogue at run time (a key
        // typed differently in code and catalogue would fall back silently).
        XCTAssertEqual(SmtpAuthDisabledGuidance.headline,
                       "Microsoft has switched off SMTP sending for this mailbox.")
    }
}
