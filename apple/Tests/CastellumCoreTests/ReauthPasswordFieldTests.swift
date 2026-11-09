// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// S75: the Re-authenticate sheet's masked, prefilled
/// password with a reveal toggle and inline re-test. Pure state only — no
/// sheet harness, real Keychain or network.
final class ReauthPasswordFieldTests: XCTestCase {
    private let secret = "hunter2-SECRET-\u{1F511}"

    private func config() -> AccountConfig {
        AccountConfig(
            accountId: "a@example.com", email: "a@example.com", host: "imap.example.com", port: 993,
            username: "a@example.com", password: secret, useTls: true,
            allowInvalidCerts: nil, smtpHost: nil, smtpPort: nil,
            smtpUsername: nil, smtpPassword: nil, smtpUseTls: nil, displayName: nil)
    }

    private func storedJSON() throws -> String { try config().encodedJSON() }

    func testStartsMaskedAndPrefilledFromStoredConfig() throws {
        let field = ReauthPasswordField(storedConfigJSON: try storedJSON())
        XCTAssertEqual(field.value, secret)
        XCTAssertFalse(field.isRevealed, "prefilled password must start masked")
    }

    func testMissingOrUndecodableConfigLeavesFieldBlankAndMasked() {
        XCTAssertEqual(ReauthPasswordField(storedConfigJSON: nil).value, "")
        XCTAssertEqual(ReauthPasswordField(storedConfigJSON: "not json").value, "")
        XCTAssertFalse(ReauthPasswordField(storedConfigJSON: nil).isRevealed)
    }

    func testToggleRevealsAndRemasks() throws {
        var field = ReauthPasswordField(storedConfigJSON: try storedJSON())
        field.toggleReveal()
        XCTAssertTrue(field.isRevealed)
        field.toggleReveal()
        XCTAssertFalse(field.isRevealed)
    }

    func testRemaskAndCloseForgetReveal() throws {
        var field = ReauthPasswordField(storedConfigJSON: try storedJSON())
        field.toggleReveal()
        field.remask()
        XCTAssertFalse(field.isRevealed, "deactivating the window re-masks")
        field.toggleReveal()
        field.close()
        XCTAssertFalse(field.isRevealed)
        XCTAssertEqual(field.value, "", "closing the sheet drops the password from view state")
    }

    func testDescriptionsAndDumpExcludeThePassword() throws {
        var field = ReauthPasswordField(storedConfigJSON: try storedJSON())
        field.toggleReveal()
        var dumped = ""
        dump(field, to: &dumped)
        for text in [field.description, field.debugDescription, "\(field)", String(reflecting: field), dumped] {
            XCTAssertFalse(text.contains(secret), "leaked: \(text)")
            XCTAssertFalse(text.contains("hunter2"), "leaked: \(text)")
        }
    }

    func testProbeResultMapping() {
        let ok = ConnectionLegResult(ok: true, errorKind: nil, summary: nil)
        let bad = ConnectionLegResult(ok: false, errorKind: "auth", summary: "x")
        let cfg = config()

        let pass = ReauthTestOutcome.map(.success(ConnectionTestReport(imap: ok, smtp: ok, mailboxes: [])), config: cfg)
        XCTAssertTrue(pass.succeeded)
        XCTAssertEqual(pass.lines.count, 2)

        let fail = ReauthTestOutcome.map(.success(ConnectionTestReport(imap: bad, smtp: ok, mailboxes: [])), config: cfg)
        XCTAssertFalse(fail.succeeded)
        XCTAssertTrue(fail.lines.first?.text.contains("password") ?? false)

        struct Boom: LocalizedError { var errorDescription: String? { "boom" } }
        let err = ReauthTestOutcome.map(.failure(Boom()), config: cfg)
        XCTAssertFalse(err.succeeded)
        XCTAssertEqual(err.lines.count, 1)
        for line in pass.lines + fail.lines + err.lines {
            XCTAssertFalse(line.text.contains(secret))
        }
    }

    // MARK: GUI-5: per-leg outcome and Save decision

    func testLegOutcomesReportOnlyTheirOwnServer() {
        let ok = ConnectionLegResult(ok: true, errorKind: nil, summary: nil)
        let bad = ConnectionLegResult(ok: false, errorKind: "auth", summary: "x")
        let cfg = config()
        let smtpBad = ConnectionTestReport(imap: ok, smtp: bad, mailboxes: [])

        let imap = ReauthTestOutcome.map(.success(smtpBad), config: cfg, leg: .imap)
        XCTAssertTrue(imap.succeeded)
        XCTAssertEqual(imap.lines.count, 1)
        XCTAssertTrue(imap.lines[0].text.contains("Incoming"))

        let smtp = ReauthTestOutcome.map(.success(smtpBad), config: cfg, leg: .smtp)
        XCTAssertFalse(smtp.succeeded)
        XCTAssertEqual(smtp.lines.count, 1)
        XCTAssertTrue(smtp.lines[0].text.contains("SMTP"))

        XCTAssertFalse(ReauthTestOutcome.map(.success(smtpBad), config: cfg).succeeded)
    }

    func testSMTPLegWithoutSMTPSettingsFailsWithExplanation() {
        let ok = ConnectionLegResult(ok: true, errorKind: nil, summary: nil)
        let out = ReauthTestOutcome.map(
            .success(ConnectionTestReport(imap: ok, smtp: nil, mailboxes: [])), config: config(), leg: .smtp)
        XCTAssertFalse(out.succeeded)
        XCTAssertEqual(out.lines.count, 1)
    }

    func testSaveDecisionOnlyWarnsAfterAKnownFailure() {
        let pass = ReauthTestOutcome(succeeded: true, lines: [])
        let fail = ReauthTestOutcome(succeeded: false, lines: [])
        XCTAssertEqual(ReauthSaveDecision.decide(lastTest: nil), .saveNow)
        XCTAssertEqual(ReauthSaveDecision.decide(lastTest: pass), .saveNow)
        XCTAssertEqual(ReauthSaveDecision.decide(lastTest: fail), .confirmFailedTest)
    }

    func testMatchingIsNilOnceConnectionFieldsChange() {
        let ok = ConnectionLegResult(ok: true, errorKind: nil, summary: nil)
        let report = ConnectionTestReport(imap: ok, smtp: ok, mailboxes: [])
        let tested = config()
        var edited = tested
        edited.smtpPassword = "changed"   // an SMTP-only edit must also invalidate
        XCTAssertNotNil(ReauthTestOutcome.matching(report: report, tested: tested, current: tested))
        XCTAssertNil(ReauthTestOutcome.matching(report: report, tested: tested, current: edited))
    }
}
