// Castellum - write-safety UI policy.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure decision logic behind the per-account evaluation-mode indicator and the
// enable-writes confirmation dialog. The dialog wording is asserted here too:
// it is the only place a user is told what leaving read-only mode means, so it
// must not drift silently.

import XCTest
@testable import Castellum

final class AccountWritePolicyUITests: XCTestCase {

    // MARK: - fail-closed state resolution

    func testUnknownAccountIsReadOnly() {
        // The UI must fail closed exactly like the core: no answer yet (the
        // per-account probe has not returned) is READ-ONLY, never "writable".
        XCTAssertEqual(AccountWritePolicy.state(for: "acc1", in: [:]), .readOnly)
    }

    func testExplicitFalseIsReadOnly() {
        XCTAssertEqual(AccountWritePolicy.state(for: "acc1", in: ["acc1": false]), .readOnly)
    }

    func testExplicitTrueIsWritesEnabled() {
        XCTAssertEqual(AccountWritePolicy.state(for: "acc1", in: ["acc1": true]), .writesEnabled)
    }

    func testAnotherAccountsPolicyNeverLeaksAcrossAccounts() {
        let map = ["acc1": true, "acc2": false]
        XCTAssertEqual(AccountWritePolicy.state(for: "acc2", in: map), .readOnly)
        XCTAssertEqual(AccountWritePolicy.state(for: "acc3", in: map), .readOnly)
    }

    // MARK: - classifying the core's refusal

    func testEvaluationModeRefusalIsRecognised() {
        // The exact string the Rust core returns (castellum-db repo.rs
        // EVALUATION_MODE_MSG); its prefix is pinned by a Rust test.
        let core = CastellumError.message(
            "evaluation mode: no changes are made to the mail server (enable writes to act)")
        XCTAssertTrue(AccountWritePolicy.isEvaluationModeRefusal(core))
    }

    func testEvaluationModeRefusalMatchesOnPrefixOnly() {
        XCTAssertTrue(AccountWritePolicy.isEvaluationModeRefusal(
            CastellumError.message("evaluation mode: some future wording")))
    }

    func testOtherFailuresAreNotEvaluationMode() {
        XCTAssertFalse(AccountWritePolicy.isEvaluationModeRefusal(CastellumError.network))
        XCTAssertFalse(AccountWritePolicy.isEvaluationModeRefusal(CastellumError.io))
        XCTAssertFalse(AccountWritePolicy.isEvaluationModeRefusal(
            CastellumError.message("mailbox not found")))
        // A message that merely mentions the words must not match — only the
        // core's own leading marker counts.
        XCTAssertFalse(AccountWritePolicy.isEvaluationModeRefusal(
            CastellumError.message("archive failed in evaluation mode: retry")))
    }

    func testNonCastellumErrorsAreNotEvaluationMode() {
        struct Other: Error {}
        XCTAssertFalse(AccountWritePolicy.isEvaluationModeRefusal(Other()))
    }

    // MARK: - user-facing copy (British English, no jargon)

    func testReadOnlyBadgeIsShortAndExplained() {
        XCTAssertEqual(AccountWritePolicy.badgeLabel, "Read-only")
        XCTAssertTrue(AccountWritePolicy.badgeHelp(for: "marc@example.com")
            .contains("marc@example.com"))
    }

    func testEnableDialogNamesTheAccountAndTheConsequence() {
        let message = AccountWritePolicy.enableConfirmationMessage(email: "marc@example.com")
        XCTAssertTrue(message.contains("marc@example.com"), message)
        // The whole point of the dialog: say that the SERVER will change.
        XCTAssertTrue(message.lowercased().contains("mail server"), message)
        XCTAssertEqual(AccountWritePolicy.enableConfirmButton, "Enable Writes")
        XCTAssertTrue(AccountWritePolicy.enableConfirmationTitle.contains("Enable Writes"))
    }

    func testDisableDialogSaysWritesStopReachingTheServer() {
        let message = AccountWritePolicy.disableConfirmationMessage(email: "marc@example.com")
        XCTAssertTrue(message.contains("marc@example.com"), message)
        XCTAssertTrue(message.lowercased().contains("read-only"), message)
        XCTAssertEqual(AccountWritePolicy.disableConfirmButton, "Return to Read-only")
    }

    // MARK: - the single prompt slot

    func testEveryPromptExposesItsAccountAndEmail() {
        let prompts: [WriteSafetyPrompt] = [
            .blocked(accountId: "a1", email: "marc@example.com", verb: "Archive"),
            .confirmEnable(accountId: "a1", email: "marc@example.com"),
            .confirmDisable(accountId: "a1", email: "marc@example.com"),
        ]
        for p in prompts {
            XCTAssertEqual(p.accountId, "a1", "\(p)")
            XCTAssertEqual(p.email, "marc@example.com", "\(p)")
        }
    }

    func testPromptIdentitiesAreDistinctSoOneNeverReplacesAnother() {
        // SwiftUI keys `.alert(item:)` on `id`; two different prompts sharing an
        // id would silently suppress the second — including a refusal notice
        // being swallowed by a confirmation for the same account.
        let ids = Set([
            WriteSafetyPrompt.blocked(accountId: "a1", email: "e", verb: "Archive").id,
            WriteSafetyPrompt.blocked(accountId: "a1", email: "e", verb: "Delete").id,
            WriteSafetyPrompt.confirmEnable(accountId: "a1", email: "e").id,
            WriteSafetyPrompt.confirmDisable(accountId: "a1", email: "e").id,
            WriteSafetyPrompt.confirmEnable(accountId: "a2", email: "e").id,
        ])
        XCTAssertEqual(ids.count, 5, "all five prompts must be distinguishable: \(ids)")
    }

    func testBlockedActionNoticeNamesTheAttemptedVerb() {
        let notice = AccountWritePolicy.blockedNoticeMessage(
            verb: "Archive", email: "marc@example.com")
        XCTAssertTrue(notice.contains("Archive"), notice)
        XCTAssertTrue(notice.contains("marc@example.com"), notice)
    }
}
