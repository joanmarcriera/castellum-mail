// Castellum - S42 AppModel tests: tags (IMAP keywords with colours) and
// conversation mute.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Seeding pattern follows RefusedWriteFlickerAppModelTests (AppModel against
// a temp profile directory, per-test Keychain namespace handled automatically
// under XCTest — see TestHost.isActive) and DirectMessageFixture (there is no
// FFI seam to insert a message without a real IMAP sync, so this writes
// directly into the profile's castellum.db and reads it back through the
// real FFI path).

import XCTest
@testable import Castellum

@MainActor
final class TagsAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-tags-\(UUID().uuidString)")
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

    /// Polls `condition` until it's true or `timeout` elapses — the Task.
    /// detached write in toggleTag/toggleMuteSelectedThread lands on a later
    /// async hop than the call that starts it (same pattern as
    /// SnoozeAppModelTests.waitUntil, shorter timeout: no real I/O here).
    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 5,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    private func header(id: String, accountId: String, mailboxId: String) -> MessageHeader {
        MessageHeader(
            id: id, accountId: accountId, mailboxId: mailboxId,
            subject: "Test", fromName: nil, fromAddress: "a@example.test", sentAt: nil,
            snippet: nil, hasAttachments: false, hasBody: nil,
            isRead: false, isStarred: false, threadCount: 1)
    }

    private func seedMessage(_ model: AppModel, accountId: String, mailboxId: String, messageId: String) throws {
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId,
            email: "a@example.test", mailboxId: mailboxId)
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: messageId, accountId: accountId,
            mailboxId: mailboxId, subject: "Test", fromAddress: "a@example.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
    }

    func testCreatingATagAddsItToModelTags() {
        let model = makeModel()
        let created = model.createTag(displayName: "Work", colour: "blue")
        XCTAssertNotNil(created)
        XCTAssertTrue(model.tags.contains { $0.displayName == "Work" })
        XCTAssertEqual(created?.colour, "blue")
    }

    /// Adopting a keyword already in use must keep its exact server spelling;
    /// re-deriving it from the display name would rewrite "Follow/up" to
    /// "Follow_up" and orphan every message carrying the original.
    func testAdoptingAKeywordInUseKeepsItsServerSpelling() {
        let model = makeModel()
        let adopted = model.createTag(displayName: "Follow/up", colour: "red", keyword: "Follow/up")
        XCTAssertEqual(adopted?.keyword, "Follow/up")
        XCTAssertEqual(adopted?.keywordKey, "follow/up")
    }

    func testTogglingATagOnASeededMessageSetsKeywordsAndLocalOnlyNotice() async throws {
        let model = makeModel()
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        let messageId = "\(mailboxId):1"
        try seedMessage(model, accountId: accountId, mailboxId: mailboxId, messageId: messageId)

        model.messages = [header(id: messageId, accountId: accountId, mailboxId: mailboxId)]
        model.selectedMessageIds = [messageId]

        model.toggleTag("Work")
        // Applied synchronously, before the background write has any chance
        // to run — the same "checked before Task.detached lands" window
        // RefusedWriteFlickerAppModelTests exercises for delete/archive.
        XCTAssertEqual(
            model.messages[0].keywords, ["Work"],
            "the optimistic update applies synchronously")

        // A fresh/unprobed account defaults to evaluation (read-only) mode
        // (AccountWritePolicy's fail-closed rule), so the write comes back
        // synced == false, reason == "evaluation_mode".
        try await waitUntil({ model.lastTagLocalOnlyNotice != nil }, "no local-only notice arrived")
        XCTAssertTrue(
            model.lastTagLocalOnlyNotice?.contains("evaluation mode") ?? false,
            "expected the evaluation-mode notice, got: \(model.lastTagLocalOnlyNotice ?? "nil")")
        // Nothing reached the server, so there is nothing to revert — the
        // keyword stays on the message, kept on this Mac only.
        XCTAssertEqual(model.messages[0].keywords, ["Work"])
    }

    func testTogglingTheSameTagAgainRemovesIt() async throws {
        let model = makeModel()
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        let messageId = "\(mailboxId):1"
        try seedMessage(model, accountId: accountId, mailboxId: mailboxId, messageId: messageId)

        model.messages = [header(id: messageId, accountId: accountId, mailboxId: mailboxId)]
        model.selectedMessageIds = [messageId]

        model.toggleTag("Work")
        try await waitUntil({ model.lastTagLocalOnlyNotice != nil }, "no local-only notice after add")
        XCTAssertEqual(model.messages[0].keywords, ["Work"])

        model.lastTagLocalOnlyNotice = nil
        model.toggleTag("Work")
        XCTAssertEqual(
            model.messages[0].keywords, [],
            "toggling a keyword every selected message already has removes it")
    }

    /// FP43: removing a server-origin tag in evaluation mode only hides it
    /// here (the next sync restores it), so the notice must not say "kept".
    func testRemovingServerTagInEvaluationModeSaysRemovedNotKept() async throws {
        let model = makeModel()
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        let messageId = "\(mailboxId):1"
        try seedMessage(model, accountId: accountId, mailboxId: mailboxId, messageId: messageId)

        try DirectFixture.seedServerKeyword(
            profileDirectory: model.profileDirectory, messageId: messageId, keyword: "Work")
        var h = header(id: messageId, accountId: accountId, mailboxId: mailboxId)
        h.keywords = ["Work"]
        model.messages = [h]
        model.selectedMessageIds = [messageId]

        model.toggleTag("Work")
        XCTAssertEqual(model.messages[0].keywords, [])
        try await waitUntil({ model.lastTagLocalOnlyNotice != nil }, "no notice arrived")
        let notice = model.lastTagLocalOnlyNotice ?? ""
        XCTAssertFalse(notice.contains("kept"), notice)
        XCTAssertTrue(notice.contains("server") || notice.contains("sync"), notice)
    }

    /// #3457: a failed write must restore each message's OWN prior tags. A mixed
    /// selection adds the tag to both; when the write for the message that
    /// already had it fails (purged mid-action: unknown id), the revert used to
    /// remove the tag it carried all along.
    func testFailedTagWriteRestoresTheMessagesPriorTagNotTheInverse() async throws {
        let model = makeModel()
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        let seeded = "\(mailboxId):1"
        let ghost = "\(mailboxId):2"  // in the list, absent from the database
        try seedMessage(model, accountId: accountId, mailboxId: mailboxId, messageId: seeded)

        var tagged = header(id: ghost, accountId: accountId, mailboxId: mailboxId)
        tagged.keywords = ["Work"]
        model.messages = [header(id: seeded, accountId: accountId, mailboxId: mailboxId), tagged]
        model.selectedMessageIds = [seeded, ghost]

        model.toggleTag("Work")  // `seeded` lacks it, so this is an add for both
        // The already-tagged message is not written at all any more (#3457), so
        // there is no failure to wait for: give the detached writes a moment.
        try await waitUntil({ true }, timeout: 0.3, "unreachable")
        let afterGhost = model.messages.first { $0.id == ghost }?.keywords
        XCTAssertEqual(afterGhost, ["Work"], "the message that already had the tag keeps it")
        XCTAssertNil(model.errorText, "no write was attempted for the unchanged message")
    }

    /// #3457: a failing write for a message that did NOT have the tag reverts it
    /// to its own prior state (no tag), and a mixed selection's other message is
    /// untouched.
    func testFailedTagWriteForAnUntaggedMessageRevertsOnlyThatMessage() async throws {
        let model = makeModel()
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        let ghost = "\(mailboxId):2"  // in the list, absent from the database
        var tagged = header(id: "\(mailboxId):3", accountId: accountId, mailboxId: mailboxId)
        tagged.keywords = ["Work"]
        model.messages = [header(id: ghost, accountId: accountId, mailboxId: mailboxId), tagged]
        model.selectedMessageIds = [ghost, tagged.id]

        model.toggleTag("Work")  // ghost lacks it: add for both, ghost's write fails
        try await waitUntil({ model.errorText != nil || model.lastTagLocalOnlyNotice != nil },
                            "the failing write never settled")
        XCTAssertEqual(model.messages.first { $0.id == ghost }?.keywords, [])
        XCTAssertEqual(model.messages.first { $0.id == tagged.id }?.keywords, ["Work"])
    }

    /// #3441: a refused/failed star write restores each slot's own prior state. The
    /// list row is already starred (e.g. after a sync reload) while the selected
    /// header is stale and unstarred; the failed write must not unstar the row.
    func testFailedStarWriteRestoresEachSlotsOwnPriorState() async throws {
        let model = makeModel()
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        let ghost = "\(mailboxId):9"  // in the list, absent from the database
        var row = header(id: ghost, accountId: accountId, mailboxId: mailboxId)
        row.isStarred = true
        model.messages = [row]
        model.selectedMessage = header(id: ghost, accountId: accountId, mailboxId: mailboxId)

        model.setStarred(ids: [ghost], starred: true)  // the stale selection disagrees
        try await waitUntil({ model.errorText != nil }, "the failing write never settled")
        XCTAssertTrue(model.messages[0].isStarred, "the row was starred before and stays starred")
        XCTAssertEqual(model.selectedMessage?.isStarred, false, "the stale header goes back to its own state")
    }

    /// #3459: with rows from two accounts, the account that REFUSED is blamed, not the
    /// first row's. A is write-enabled and first in the list; B is read-only.
    func testStarRefusalBlamesTheRefusingAccountNotTheFirstRow() async throws {
        let model = makeModel()
        let a = "acct-a-\(UUID().uuidString)", b = "acct-b-\(UUID().uuidString)"
        let (mbA, mbB) = ("\(a):INBOX", "\(b):INBOX")
        for (acct, mb, email) in [(a, mbA, "a@example.test"), (b, mbB, "b@example.test")] {
            try DirectFixture.seedAccountAndMailbox(
                profileDirectory: model.profileDirectory, accountId: acct, email: email, mailboxId: mb)
            try DirectFixture.seedMessage(
                profileDirectory: model.profileDirectory, id: "\(mb):1", accountId: acct, mailboxId: mb,
                subject: "Test", fromAddress: "x@example.test",
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        }
        try model.core.setAccountWriteEnabled(accountId: a, enabled: true)
        model.accounts = [
            Account(id: a, email: "a@example.test", displayName: nil, provider: "imap"),
            Account(id: b, email: "b@example.test", displayName: nil, provider: "imap"),
        ]
        model.messages = [
            header(id: "\(mbA):1", accountId: a, mailboxId: mbA),
            header(id: "\(mbB):1", accountId: b, mailboxId: mbB),
        ]

        model.setStarred(ids: ["\(mbA):1", "\(mbB):1"], starred: true)
        try await waitUntil({ model.writeSafetyPrompt != nil }, "no refusal prompt arrived")

        guard case .blocked(let blamed, _, _)? = model.writeSafetyPrompt else {
            return XCTFail("expected a blocked prompt, got \(String(describing: model.writeSafetyPrompt))")
        }
        XCTAssertEqual(blamed, b, "the read-only account is the one named")
        XCTAssertNotEqual(model.accountWriteEnabled[a], false, "account A was not marked read-only")
        XCTAssertTrue(model.messages.first { $0.accountId == a }?.isStarred ?? false)
        XCTAssertFalse(model.messages.first { $0.accountId == b }?.isStarred ?? true, "B's row is reverted")
    }

    func testMutingASeededMessageSetsThreadMutedAndUnmuteReverses() async throws {
        let model = makeModel()
        let accountId = "acct-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        let messageId = "\(mailboxId):1"
        try seedMessage(model, accountId: accountId, mailboxId: mailboxId, messageId: messageId)

        model.messages = [header(id: messageId, accountId: accountId, mailboxId: mailboxId)]
        model.selectedMessageIds = [messageId]
        XCTAssertFalse(model.messages[0].threadMuted)

        model.toggleMuteSelectedThread()
        XCTAssertTrue(
            model.messages[0].threadMuted,
            "the optimistic update applies synchronously")
        // Mute is local-only and never refused — give the detached write a
        // moment, then confirm it did NOT get reverted.
        try await waitUntil({ true }, timeout: 0.2, "unreachable")
        XCTAssertTrue(model.messages[0].threadMuted)

        model.toggleMuteSelectedThread()
        XCTAssertFalse(model.messages[0].threadMuted)
    }

    func testDecodingAHeaderJSONWithoutTheNewKeysSucceedsWithDefaults() throws {
        let json = """
        {"id":"a:INBOX:1","accountId":"a","mailboxId":"a:INBOX","subject":null,
         "fromName":null,"fromAddress":null,"sentAt":null,"snippet":null,
         "hasAttachments":false,"hasBody":null,"isRead":false,"isStarred":false,
         "threadCount":1}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let decodedHeader = try decoder.decode(MessageHeader.self, from: Data(json.utf8))
        XCTAssertEqual(decodedHeader.keywords, [])
        XCTAssertFalse(decodedHeader.threadMuted)
    }
}
