// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

@MainActor
final class ComposeSessionTests: XCTestCase {
    func testEmptySessionHasNoContentAndRefusesSend() {
        let s = ComposeSession(accountId: "acct")
        XCTAssertFalse(s.hasContent)
        XCTAssertFalse(s.hasUserEdits)
        XCTAssertEqual(s.closeAction, .none)
        let (draft, problem) = s.buildDraftForSend()
        XCTAssertNil(draft)
        XCTAssertNotNil(problem)
    }

    func testUntouchedReplyPrefillDoesNotRequestDraftSave() {
        let s = ComposeSession(
            accountId: "acct",
            prefill: ReplyPrefill(
                to: [DraftAddress(name: "Ada", email: "ada@x.test")],
                subject: "Re: Plans",
                inReplyTo: "<orig@x.test>",
                referencesHeader: "<orig@x.test>",
                body: "\n\nAda wrote:\n> Original"
            )
        )

        XCTAssertTrue(s.hasContent)
        XCTAssertFalse(s.hasUserEdits)
        XCTAssertEqual(s.closeAction, .none)
    }

    func testEditingReplyPrefillRequestsDraftSave() {
        let s = ComposeSession(
            accountId: "acct",
            prefill: ReplyPrefill(
                to: [DraftAddress(name: "Ada", email: "ada@x.test")],
                subject: "Re: Plans",
                inReplyTo: "<orig@x.test>",
                referencesHeader: "<orig@x.test>",
                body: "\n\nAda wrote:\n> Original"
            )
        )

        s.body = "Thanks." + s.body

        XCTAssertTrue(s.hasUserEdits)
        XCTAssertEqual(s.closeAction, .save)
    }

    func testEditedNewMessageStillRequestsDraftSave() {
        let s = ComposeSession(accountId: "acct")

        s.toText = "ada@x.test"
        s.body = "Work in progress"

        XCTAssertTrue(s.hasUserEdits)
        XCTAssertTrue(s.hasContent)
        XCTAssertEqual(s.closeAction, .save)
    }

    func testRevertingReplyToItsInitialStateDoesNotSave() {
        let s = ComposeSession(
            accountId: "acct",
            prefill: ReplyPrefill(
                to: [DraftAddress(name: "Ada", email: "ada@x.test")],
                subject: "Re: Plans",
                inReplyTo: nil,
                referencesHeader: nil,
                body: "\n\n> Original"
            )
        )
        let originalSubject = s.subject

        s.subject = "Changed"
        XCTAssertEqual(s.closeAction, .save)
        s.subject = originalSubject

        XCTAssertFalse(s.hasUserEdits)
        XCTAssertEqual(s.closeAction, .none)
    }

    func testSendRequiresAtLeastOneToRecipient() {
        let s = ComposeSession(accountId: "acct")
        s.subject = "Hi"
        s.body = "There"
        let (draft, problem) = s.buildDraftForSend()
        XCTAssertNil(draft)
        XCTAssertEqual(problem, "Add at least one To recipient.")
    }

    func testSendRejectsInvalidEntriesWithMessage() {
        let s = ComposeSession(accountId: "acct")
        s.toText = "ada@x.test, nonsense"
        let (draft, problem) = s.buildDraftForSend()
        XCTAssertNil(draft)
        XCTAssertEqual(problem, "Not a valid address: nonsense")
    }

    func testBuildDraftForSendCarriesAllFields() throws {
        let s = ComposeSession(accountId: "acct",
                               prefill: ReplyPrefill(to: [DraftAddress(name: "Ada", email: "ada@x.test")],
                                                     subject: "Re: Plans",
                                                     inReplyTo: "<orig@x.test>",
                                                     referencesHeader: "<orig@x.test>"))
        s.ccText = "bob@x.test"
        s.body = "Sounds good."
        s.draftId = 9
        let draft = try XCTUnwrap(s.buildDraftForSend().draft)
        XCTAssertEqual(draft.id, 9)
        XCTAssertEqual(draft.accountId, "acct")
        XCTAssertEqual(draft.to, [DraftAddress(name: "Ada", email: "ada@x.test")])
        XCTAssertEqual(draft.cc, [DraftAddress(name: nil, email: "bob@x.test")])
        XCTAssertEqual(draft.subject, "Re: Plans")
        XCTAssertEqual(draft.bodyText, "Sounds good.")
        XCTAssertEqual(draft.inReplyTo, "<orig@x.test>")
        XCTAssertEqual(draft.referencesHeader, "<orig@x.test>")
    }

    func testBuildDraftForSaveIsLenientAboutInvalidEntries() {
        let s = ComposeSession(accountId: "acct")
        s.toText = "ada@x.test, half-typed"
        s.body = "wip"
        let draft = s.buildDraftForSave()
        XCTAssertEqual(draft.to, [DraftAddress(name: nil, email: "ada@x.test")])
        XCTAssertEqual(draft.bodyText, "wip")
    }

    func testSessionFromDraftRoundTripsFields() {
        let stored = Draft(id: 3, accountId: "acct",
                           to: [DraftAddress(name: "Lovelace, Ada", email: "ada@x.test")],
                           cc: [], bcc: [DraftAddress(name: nil, email: "b@x.test")],
                           subject: "Hi", bodyText: "Body", bodyHtml: nil,
                           fromEmail: nil, fromName: nil,
                           inReplyTo: "<o@x>", referencesHeader: "<o@x>",
                           status: "draft", updatedAtEpoch: 0,
                           serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
                           serverMessageId: nil, serverSavedAtEpoch: nil)
        let s = ComposeSession(draft: stored)
        XCTAssertEqual(s.draftId, 3)
        XCTAssertFalse(s.hasUserEdits)
        XCTAssertEqual(s.closeAction, .none)
        XCTAssertEqual(s.toText, #""Lovelace, Ada" <ada@x.test>"#)
        XCTAssertEqual(s.bccText, "b@x.test")
        let draft = s.buildDraftForSave()
        XCTAssertEqual(draft.id, 3)
        XCTAssertEqual(draft.to, stored.to)
        XCTAssertEqual(draft.bcc, stored.bcc)
        XCTAssertEqual(draft.inReplyTo, "<o@x>")
    }

    func testExistingDraftEditedDownToEmptyRequestsDeletion() {
        let stored = Draft(id: 3, accountId: "acct",
                           to: [DraftAddress(name: nil, email: "ada@x.test")],
                           cc: [], bcc: [], subject: "Hi", bodyText: "Body", bodyHtml: nil,
                           fromEmail: nil, fromName: nil,
                           inReplyTo: nil, referencesHeader: nil,
                           status: "draft", updatedAtEpoch: 0,
                           serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
                           serverMessageId: nil, serverSavedAtEpoch: nil)
        let s = ComposeSession(draft: stored)

        s.toText = ""
        s.subject = ""
        s.body = ""

        XCTAssertTrue(s.hasUserEdits)
        XCTAssertFalse(s.hasContent)
        XCTAssertEqual(s.closeAction, .deleteDraft(3))
    }

    // MARK: FP07

    private func storedDraft(inReplyTo: String? = nil, subject: String = "Hi") -> Draft {
        Draft(id: 7, accountId: "acct",
              to: [DraftAddress(name: nil, email: "ada@x.test")],
              cc: [], bcc: [], subject: subject, bodyText: "Body", bodyHtml: nil,
              fromEmail: nil, fromName: nil,
              inReplyTo: inReplyTo, referencesHeader: nil,
              status: "draft", updatedAtEpoch: 0,
              serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
              serverMessageId: nil, serverSavedAtEpoch: nil)
    }

    func testNewSessionTypedThenErasedDeletesItsOwnDraftRow() {
        let s = ComposeSession(accountId: "acct")
        s.draftId = 7            // the early save created the row
        s.subject = "x"
        s.body = "Half a thought."
        XCTAssertEqual(s.closeAction, .save)
        s.subject = ""
        s.body = ""
        XCTAssertEqual(s.closeAction, .deleteDraft(7))
    }

    func testRevertedReplyWithOwnDraftRowDeletesIt() {
        let s = ComposeSession(accountId: "acct", prefill: ReplyPrefill(
            to: [DraftAddress(name: "Ada", email: "ada@x.test")], subject: "Re: Plans",
            inReplyTo: nil, referencesHeader: nil, body: "\n\n> Original"))
        s.draftId = 7
        let original = s.subject
        s.subject = "Changed"
        s.subject = original
        XCTAssertEqual(s.closeAction, .deleteDraft(7))
    }

    func testUntouchedStoredDraftIsKept() {
        let s = ComposeSession(draft: storedDraft())
        XCTAssertEqual(s.closeAction, .none)
    }

    // MARK: — From / Markdown choices on a reopened draft

    /// The From picker changes neither the account nor the body, so a
    /// From-only change used to compare equal to the opening snapshot and
    /// Cmd-W dropped it.
    func testChangingOnlyFromOnAStoredDraftRequestsSave() {
        let s = ComposeSession(draft: storedDraft())
        s.userChoseFrom(ComposeIdentity(email: "alias@x.test", name: "Alias"))
        XCTAssertEqual(s.closeAction, .save)
        XCTAssertEqual(s.buildDraftForSave().fromEmail, "alias@x.test")
    }

    func testRePickingTheSameFromOnAStoredDraftIsNotAnEdit() {
        let s = ComposeSession(draft: storedDraft())
        s.userChoseFrom(nil)   // the account's own address, as stored
        XCTAssertEqual(s.closeAction, .none)
        s.userChoseFrom(ComposeIdentity(email: "alias@x.test", name: "Alias"))
        s.userChoseFrom(nil)
        XCTAssertEqual(s.closeAction, .none, "back to the stored From")
    }

    func testTogglingOnlyMarkdownOnAStoredDraftRequestsSave() {
        let s = ComposeSession(draft: storedDraft())
        XCTAssertFalse(s.markdownEnabled)
        s.markdownEnabled = true
        XCTAssertEqual(s.closeAction, .save)
        s.markdownEnabled = false
        XCTAssertEqual(s.closeAction, .none)
    }

    /// The automatic From choice on a fresh reply is not the user's edit:
    /// "an untouched fresh reply is a no-op" must still hold.
    func testAutoChosenFromOnAFreshReplyIsStillANoOp() {
        let s = ComposeSession(accountId: "acct", prefill: ReplyPrefill(
            to: [DraftAddress(name: "Ada", email: "ada@x.test")], subject: "Re: Plans",
            inReplyTo: nil, referencesHeader: nil, body: "\n\n> Original"))
        s.autoSelectIdentity(identities: [], accountEmail: "me@x.test")
        XCTAssertEqual(s.closeAction, .none)
        s.userChoseFrom(ComposeIdentity(email: "alias@x.test", name: nil))
        XCTAssertEqual(s.closeAction, .none)
    }

    func testInheritedSubjectIsNotScannedForForgottenAttachment() {
        let suite = "ComposeSessionTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        let prefs = Preferences(defaults: defaults)
        let s = ComposeSession(accountId: "acct", prefill: ReplyPrefill(
            to: [DraftAddress(name: "Ada", email: "ada@x.test")], subject: "Re: attached contract",
            inReplyTo: nil, referencesHeader: nil, body: ""), preferences: prefs)
        s.body = "Thanks, agreed."
        XCTAssertNil(s.forgottenAttachmentWord())
        s.subject = "Re: attached contract, signed copy attached"
        XCTAssertNotNil(s.forgottenAttachmentWord())
        let d = ComposeSession(draft: storedDraft(inReplyTo: "<a@x>", subject: "Re: attached"),
                               preferences: prefs)
        XCTAssertNil(d.forgottenAttachmentWord())
    }

    func testSendDeduplicatesRecipientsAcrossFields() throws {
        let s = ComposeSession(accountId: "acct")
        s.toText = "bob@x.test, bob@x.test, Bob Ferrer <BOB@x.test>, ann@x.test"
        s.ccText = "bob@x.test, cy@x.test"
        s.bccText = "ANN@x.test"
        let d = try XCTUnwrap(s.buildDraftForSend().draft)
        XCTAssertEqual(d.to.map { $0.email.lowercased() }, ["bob@x.test", "ann@x.test"])
        XCTAssertEqual(d.to.first?.name, "Bob Ferrer")
        XCTAssertEqual(d.cc.map(\.email), ["cy@x.test"])
        XCTAssertTrue(d.bcc.isEmpty)
    }
}
