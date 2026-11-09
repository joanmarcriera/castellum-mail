// Castellum - DR1: the Drafts list shows each draft's subject and RECIPIENT
// (never the account owner as sender), and opening a draft (double-click or
// Return) restores to, cc, subject, body and attachments in the composer.
// Covers the three draft shapes: local-only, server-saved, and the one a
// refused send leaves behind. Temp profile only: no real profile, Keychain
// namespace or mail server.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class DraftListTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-dr1-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(_ condition: () -> Bool, _ message: String) async throws {
        let deadline = Date().addingTimeInterval(5)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        if !condition() { XCTFail(message) }
    }

    private let bob = DraftAddress(name: "Bob Builder", email: "bob@example.com")
    private let carol = DraftAddress(name: nil, email: "carol@example.com")

    private func draftsMailbox() -> Mailbox {
        Mailbox(
            id: "acc:Drafts", accountId: "acc", path: "INBOX", name: "Drafts", role: "drafts",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    private func seedAccount(_ model: AppModel) throws {
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: "acc", email: "me@local.test",
            mailboxId: "acc:Drafts", role: "drafts")
    }

    private func saveDraft(
        _ model: AppModel, subject: String, to: [DraftAddress], cc: [DraftAddress] = [],
        body: String = "Body text"
    ) throws -> Draft {
        let id = try model.core.saveDraft(
            NewDraft(accountId: "acc", to: to, cc: cc, subject: subject, bodyText: body))
        return try XCTUnwrap(try model.core.listDrafts(accountId: "acc").first { $0.id == id })
    }

    // MARK: rows

    func testLocalDraftRowShowsSubjectAndRecipient() throws {
        let model = makeModel()
        try seedAccount(model)
        let draft = try saveDraft(model, subject: "Quarterly plan", to: [bob, carol])
        XCTAssertNil(draft.serverMailboxPath, "sanity: local-only")
        let row = DraftListRow(draft: draft)
        XCTAssertEqual(row.subject, "Quarterly plan")
        XCTAssertEqual(row.recipient, "Bob Builder, carol@example.com")
    }

    func testDraftWithoutSubjectOrRecipientGetsPlaceholders() throws {
        let model = makeModel()
        try seedAccount(model)
        let row = DraftListRow(draft: try saveDraft(model, subject: "", to: []))
        XCTAssertEqual(row.subject, "(no subject)")
        XCTAssertEqual(row.recipient, "(no recipient)")
    }

    /// An account still in evaluation mode refuses the send; the draft is
    /// left behind and must still read as subject + recipient.
    func testDraftLeftByARefusedSendStillShowsSubjectAndRecipient() throws {
        let model = makeModel()
        try seedAccount(model)
        let draft = try saveDraft(model, subject: "Refused send", to: [bob])
        XCTAssertThrowsError(try model.core.sendDraft(id: draft.id), "evaluation mode must refuse")
        let left = try XCTUnwrap(try model.core.listDrafts(accountId: "acc").first { $0.id == draft.id })
        XCTAssertNotEqual(left.status, "sent")
        let row = DraftListRow(draft: left)
        XCTAssertEqual(row.subject, "Refused send")
        XCTAssertEqual(row.recipient, "Bob Builder")
    }

    /// The server-saved copy is a message in the Drafts mailbox: its list
    /// header carries the recipient and the "from" cell shows it instead of
    /// the account owner.
    func testServerSavedDraftShowsSubjectAndRecipientInTheMailboxList() async throws {
        let model = makeModel()
        try seedAccount(model)
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: "acc:Drafts:1", accountId: "acc",
            mailboxId: "acc:Drafts", subject: "Server draft", fromAddress: "me@local.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        try DirectFixture.seedAddress(
            profileDirectory: model.profileDirectory, messageId: "acc:Drafts:1", kind: "to",
            name: "Bob Builder", address: "bob@example.com")
        model.select(mailbox: draftsMailbox())
        try await waitUntil({ !model.messages.isEmpty }, "Drafts list never loaded")
        let header = model.messages[0]
        XCTAssertEqual(header.subject, "Server draft")
        let row = DraftListRow(header: header)
        XCTAssertEqual(row.subject, "Server draft")
        XCTAssertEqual(row.recipient, "Bob Builder")
        XCTAssertEqual(messageListSenderText(header), "To: Bob Builder",
                       "Drafts rows show the recipient, not me@local.test")
    }

    func testInboxRowStillShowsTheSender() {
        let h = MessageHeader(
            id: "m", accountId: "acc", mailboxId: "acc:INBOX", subject: "Hi", fromName: "Alice",
            fromAddress: "alice@local.test", sentAt: 1, snippet: nil, hasAttachments: false,
            hasBody: nil, isRead: true, isStarred: false, threadCount: 1)
        XCTAssertEqual(messageListSenderText(h), "Alice")
    }

    // MARK: opening

    /// A draft with to, cc, subject, body and an attachment, mirrored to the
    /// server Drafts mailbox. Opening its list row (double-click / Return)
    /// must hand the composer the SAME local draft, fully restored.
    func testOpeningADraftRowRestoresFieldsBodyAndAttachments() async throws {
        let model = makeModel()
        try seedAccount(model)
        let draft = try saveDraft(
            model, subject: "Quarterly plan", to: [bob], cc: [carol], body: "Numbers attached.")
        _ = try model.core.addDraftAttachment(
            draftId: draft.id,
            attachment: NewDraftAttachment(
                filename: "plan.txt", contentType: "text/plain",
                dataBase64: Data("hello".utf8).base64EncodedString()))
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: "acc:Drafts:7", accountId: "acc",
            mailboxId: "acc:Drafts", subject: "Quarterly plan", fromAddress: "me@local.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        try DirectFixture.setProviderUid(
            profileDirectory: model.profileDirectory, messageId: "acc:Drafts:7", uid: 7)
        try DirectFixture.markDraftServerSaved(
            profileDirectory: model.profileDirectory, draftId: draft.id, path: "INBOX", uid: 7)
        model.select(mailbox: draftsMailbox())
        try await waitUntil({ !model.messages.isEmpty }, "Drafts list never loaded")

        var opened: Draft?
        model.onUndoReopenDraft = { opened = $0 }
        XCTAssertTrue(model.openDraft(from: model.messages[0]), "a Drafts row must be handled as a draft")
        try await waitUntil({ opened != nil }, "the composer never received the draft")

        let session = ComposeSession(draft: try XCTUnwrap(opened))
        XCTAssertEqual(opened?.id, draft.id, "must reopen the existing draft, not a duplicate")
        XCTAssertEqual(session.toText, "Bob Builder <bob@example.com>")
        XCTAssertEqual(session.ccText, "carol@example.com")
        XCTAssertEqual(session.subject, "Quarterly plan")
        XCTAssertEqual(session.body, "Numbers attached.")
        session.loadAttachments(model: model)
        try await waitUntil({ session.attachments.count == 1 }, "attachment not restored")
        XCTAssertEqual(session.attachments.first?.filename, "plan.txt")
    }

    func testOpeningAnInboxRowIsNotTreatedAsADraft() async throws {
        let model = makeModel()
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: "acc", email: "me@local.test",
            mailboxId: "acc:INBOX", role: "inbox")
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: "acc:INBOX:1", accountId: "acc",
            mailboxId: "acc:INBOX", subject: "Hello", fromAddress: "a@local.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        model.select(mailbox: Mailbox(
            id: "acc:INBOX", accountId: "acc", path: "INBOX", name: "INBOX", role: "inbox",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true))
        try await waitUntil({ !model.messages.isEmpty }, "list never loaded")
        XCTAssertFalse(model.openDraft(from: model.messages[0]))
    }
}
