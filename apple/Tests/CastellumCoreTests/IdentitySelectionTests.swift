// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// MM-6: which From a new compose window starts with, and the
/// identity extras against the real core.
@MainActor
final class IdentitySelectionTests: XCTestCase {
    private func identity(_ id: Int64, _ email: String, name: String? = nil, isDefault: Bool = false) -> Identity {
        Identity(id: id, accountId: "acc", email: email, displayName: name, isDefault: isDefault)
    }

    private let own = "me@example.com"

    func testReplyUsesTheAddressTheMailWasWrittenTo() {
        let ids = [identity(1, "work@example.com", name: "Me (work)"), identity(2, "shop@example.com")]
        let chosen = IdentitySelector.choose(
            context: .reply(recipients: ["someone@else.test", "SHOP@example.com"]), accountEmail: own, identities: ids)
        XCTAssertEqual(chosen, ComposeIdentity(email: "shop@example.com", name: nil), "case-insensitive match, keeps stored casing")
    }

    func testToBeatsCcAndListOrderDecides() {
        let ids = [identity(1, "a@example.com"), identity(2, "b@example.com")]
        XCTAssertEqual(
            IdentitySelector.choose(context: .reply(recipients: ["b@example.com", "a@example.com"]), accountEmail: own, identities: ids)?.email,
            "b@example.com")
    }

    func testAddressedToTheAccountsOwnAddressBeatsTheDefaultAlias() {
        let ids = [identity(1, "work@example.com", isDefault: true)]
        XCTAssertNil(
            IdentitySelector.choose(context: .reply(recipients: ["ME@example.com"]), accountEmail: own, identities: ids),
            "answering as the address you were written to wins over 'default for new mail'")
    }

    /// Review of PR #206: the plus-tag check ran before the identity lookup,
    /// so a plus-address identity could never be chosen for a reply.
    func testAPlusAddressIdentityIsChosenForMailSentToIt() {
        let ids = [identity(1, "me+work@example.com", name: "Me (work)"), identity(2, "shop@example.com", isDefault: true)]
        XCTAssertEqual(
            IdentitySelector.choose(context: .reply(recipients: ["ME+WORK@example.com"]), accountEmail: own, identities: ids),
            ComposeIdentity(email: "me+work@example.com", name: "Me (work)"))
        // A plus tag no identity claims is still the account's own mailbox.
        XCTAssertNil(
            IdentitySelector.choose(context: .reply(recipients: ["me+news@example.com"]), accountEmail: own, identities: ids))
    }

    func testReplyToSomethingNotAddressedToAnyOfMyAddressesFallsBackToTheDefault() {
        let ids = [identity(1, "work@example.com", name: "W", isDefault: true), identity(2, "shop@example.com")]
        XCTAssertEqual(
            IdentitySelector.choose(context: .reply(recipients: ["list@somewhere.test"]), accountEmail: own, identities: ids),
            ComposeIdentity(email: "work@example.com", name: "W"))
    }

    func testNewMessageAndForwardUseTheDefaultElseTheAccountAddress() {
        let ids = [identity(1, "work@example.com", isDefault: true)]
        XCTAssertEqual(IdentitySelector.choose(context: .newMessage, accountEmail: own, identities: ids)?.email, "work@example.com")
        XCTAssertNil(IdentitySelector.choose(context: .newMessage, accountEmail: own, identities: [identity(1, "work@example.com")]))
        XCTAssertNil(IdentitySelector.choose(context: .newMessage, accountEmail: own, identities: []))
    }

    func testSessionChoosesOnceAndNeverOverridesTheUsersPick() {
        let defaults = UserDefaults(suiteName: "identity-tests-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        let ids = [identity(1, "work@example.com", isDefault: true)]
        let auto = ComposeSession(accountId: "acc", preferences: prefs)
        auto.autoSelectIdentity(identities: ids, accountEmail: own)
        XCTAssertEqual(auto.fromIdentity?.email, "work@example.com")
        auto.userChoseFrom(nil)
        auto.autoSelectIdentity(identities: ids, accountEmail: own)
        XCTAssertNil(auto.fromIdentity, "already chosen: no second automatic pick")

        let manual = ComposeSession(accountId: "acc", preferences: prefs)
        manual.userChoseFrom(ComposeIdentity(email: "shop@example.com", name: nil))
        manual.autoSelectIdentity(identities: ids, accountEmail: own)
        XCTAssertEqual(manual.fromIdentity?.email, "shop@example.com", "a hand-picked From is never replaced")
    }

    func testReplySessionRemembersWhoTheMailWasWrittenTo() {
        let defaults = UserDefaults(suiteName: "identity-tests-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        let prefill = ReplyPrefill(
            to: [DraftAddress(name: nil, email: "them@x.test")], subject: "Re: hi", inReplyTo: "<m@x>",
            referencesHeader: "<m@x>", originalDbId: "msg-1", originalRecipients: ["shop@example.com"])
        let session = ComposeSession(accountId: "acc", prefill: prefill, preferences: prefs)
        session.autoSelectIdentity(identities: [identity(2, "shop@example.com", name: "Shop")], accountEmail: own)
        XCTAssertEqual(session.fromIdentity, ComposeIdentity(email: "shop@example.com", name: "Shop"))

        let forward = ReplyPrefill(to: [], subject: "Fwd: x", inReplyTo: nil, referencesHeader: nil, originalDbId: "msg-1", isForward: true, originalRecipients: ["shop@example.com"])
        let fwd = ComposeSession(accountId: "acc", prefill: forward, preferences: prefs)
        fwd.autoSelectIdentity(identities: [identity(2, "shop@example.com")], accountEmail: own)
        XCTAssertNil(fwd.fromIdentity, "a forward is a new message: default, not 'addressed to'")
    }

    func testPrefillCollectsToThenCcRecipientsForAReply() {
        let detail = MessageDetail(
            header: MessageHeader(id: "m", accountId: "acc", mailboxId: "acc:INBOX", subject: "S", fromName: nil, fromAddress: "them@x.test",
                                  sentAt: 1, snippet: nil, hasAttachments: false, hasBody: nil, isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: "<m@x>",
            addresses: [Address(kind: "cc", name: nil, address: "cc@x.test"), Address(kind: "to", name: nil, address: "shop@example.com"),
                        Address(kind: "from", name: nil, address: "them@x.test")],
            textBody: "hi", sanitizedHtml: nil)
        let prefill = ReplyPrefill.make(detail: detail, mode: .reply, selfEmail: own)
        XCTAssertEqual(prefill.originalRecipients, ["shop@example.com", "cc@x.test"])
        XCTAssertEqual(ReplyPrefill.make(detail: detail, mode: .forward, selfEmail: own).originalRecipients, [])
    }

    func testAddressFieldValidation() {
        for ok in ["", "  ", "a@x.test", " a@x.test "] { XCTAssertTrue(IdentityAddressField.isAcceptable(ok), ok) }
        for bad in ["a@x.test, b@x.test", "nope", "a b@x.test", "<a@x.test>", "a@x.test;b@x.test"] { XCTAssertFalse(IdentityAddressField.isAcceptable(bad), bad) }
    }

    func testIdentityExtrasRoundTripThroughTheRealCore() throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-swift-identities-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        // The account must exist first; a bare sync-less config save does that.
        let cfg = #"{"account_id":"acc","email":"me@example.com","host":"127.0.0.1","port":3143,"username":"me","password":"x","use_tls":false}"#
        try core.saveAccountWithoutSync(configJSON: cfg)
        let created = try core.createIdentity(
            accountId: "acc", email: "work@example.com", displayName: "Work",
            replyTo: "team@example.com", bcc: "archive@example.com", isDefault: true)
        XCTAssertEqual(created.replyTo, "team@example.com")
        XCTAssertEqual(created.bcc, "archive@example.com")
        XCTAssertTrue(created.isDefault)
        try core.updateIdentity(id: created.id, displayName: "Work 2", replyTo: "", bcc: nil, isDefault: false)
        let listed = try XCTUnwrap(try core.listIdentities(accountId: "acc").first)
        XCTAssertEqual(listed.displayName, "Work 2")
        XCTAssertNil(listed.replyTo, "an empty string clears")
        XCTAssertEqual(listed.bcc, "archive@example.com", "absent = kept")
        XCTAssertFalse(listed.isDefault)
        XCTAssertThrowsError(try core.updateIdentity(id: created.id, replyTo: "a@x.test, b@x.test"))
    }

    func testPlusTaggedOwnAddressCountsAsTheAccountAddress() {
        let ids = [identity(1, "work@example.com", isDefault: true)]
        XCTAssertNil(IdentitySelector.choose(context: .reply(recipients: ["me+shop@example.com"]), accountEmail: own, identities: ids))
        XCTAssertEqual(dropPlusTag("a+b+c@x.test"), "a@x.test")
        XCTAssertEqual(dropPlusTag("plain@x.test"), "plain@x.test")
    }
}
