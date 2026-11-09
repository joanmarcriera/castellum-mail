// Castellum - tests for MailtoParser/MailtoPrefill (S57) and the Share
// Extension handoff descriptor (SharePendingCompose).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class MailtoParserTests: XCTestCase {
    func testAddressOnly() {
        let p = MailtoParser.parse("mailto:ada@calc.test")
        XCTAssertEqual(p?.to, "ada@calc.test")
        XCTAssertEqual(p?.subject, "")
        XCTAssertEqual(p?.body, "")
    }

    func testFullQuery() {
        let p = MailtoParser.parse(
            "mailto:ada@calc.test?cc=bob@x.test&bcc=carol@x.test&subject=Hi%20there&body=Hello%20world")
        XCTAssertEqual(p?.to, "ada@calc.test")
        XCTAssertEqual(p?.cc, "bob@x.test")
        XCTAssertEqual(p?.bcc, "carol@x.test")
        XCTAssertEqual(p?.subject, "Hi there")
        XCTAssertEqual(p?.body, "Hello world")
    }

    func testMultipleToAddressesInPath() {
        let p = MailtoParser.parse("mailto:ada@calc.test,bob@x.test")
        XCTAssertEqual(p?.to, "ada@calc.test,bob@x.test")
    }

    func testToQueryItemAppendsToPathAddress() {
        let p = MailtoParser.parse("mailto:ada@calc.test?to=bob@x.test")
        XCTAssertEqual(p?.to, "ada@calc.test,bob@x.test")
    }

    func testBareSchemeWithNothingElse() {
        let p = MailtoParser.parse("mailto:")
        XCTAssertEqual(p, MailtoPrefill())
        XCTAssertTrue(p?.isEmpty ?? false)
    }

    func testNonMailtoSchemeReturnsNil() {
        XCTAssertNil(MailtoParser.parse("https://example.com"))
    }

    func testUnparsableStringReturnsNil() {
        XCTAssertNil(MailtoParser.parse(""))
    }

    func testUnknownQueryItemsAreIgnored() {
        let p = MailtoParser.parse("mailto:ada@calc.test?in-reply-to=abc&subject=Hi")
        XCTAssertEqual(p?.subject, "Hi")
        XCTAssertEqual(p?.to, "ada@calc.test")
    }
}

final class SharePendingComposeTests: XCTestCase {
    func testCodableRoundTrip() throws {
        let original = SharePendingCompose(subject: "From Safari", body: "https://example.com")
        let data = try JSONEncoder().encode(original)
        let decoded = try JSONDecoder().decode(SharePendingCompose.self, from: data)
        XCTAssertEqual(decoded, original)
    }

    func testDefaultsAreEmpty() {
        let pending = SharePendingCompose()
        XCTAssertEqual(pending.subject, "")
        XCTAssertEqual(pending.body, "")
    }

    /// Every test host is ad-hoc/unsigned (no Team ID), so the gate must shut
    /// the handoff off entirely — write/consume are silent no-ops that never
    /// touch ~/Library/Group Containers. Before the gate, this very test read
    /// the container and raised macOS's "access data from other apps" prompt
    /// on every `xcodebuild test`.
    func testConsumeWithoutContainerReturnsNilSafely() {
        XCTAssertNil(SharePendingCompose.appGroupId)
        XCTAssertNil(SharePendingCompose.consumePending())
        SharePendingCompose(subject: "x", body: "y").write()   // must not throw/crash
    }

    func testGroupAuthorizedOnlyByMatchingTeamPrefix() {
        let entitled = ["TEAMID0000.uk.co.riera.castellum"]
        XCTAssertEqual(
            SharePendingCompose.authorizedAppGroupId(teamID: "TEAMID0000", entitledGroups: entitled),
            "TEAMID0000.uk.co.riera.castellum")
        // Ad-hoc build: no Team ID, so nothing can authorize the group.
        XCTAssertNil(SharePendingCompose.authorizedAppGroupId(teamID: nil, entitledGroups: entitled))
        XCTAssertNil(SharePendingCompose.authorizedAppGroupId(teamID: "", entitledGroups: entitled))
        // Another team's signature, or the bare id an empty
        // $(TeamIdentifierPrefix) expands to in Debug.
        XCTAssertNil(SharePendingCompose.authorizedAppGroupId(teamID: "ZZZZZZZZZZ", entitledGroups: entitled))
        XCTAssertNil(SharePendingCompose.authorizedAppGroupId(
            teamID: "TEAMID0000", entitledGroups: ["uk.co.riera.castellum"]))
        // The iOS-style id needs a provisioning profile on macOS: never used.
        XCTAssertNil(SharePendingCompose.authorizedAppGroupId(
            teamID: "TEAMID0000", entitledGroups: ["group.uk.co.riera.castellum"]))
    }
}

final class ComposeSessionMailtoTests: XCTestCase {
    @MainActor
    func testInitFromMailtoPrefillSeedsFieldsAndIsNotAnEditUntilTouched() {
        let prefill = MailtoPrefill(to: "a@b.test", cc: "c@b.test", bcc: "d@b.test",
                                    subject: "Hi", body: "Hello")
        let session = ComposeSession(accountId: "acct1", mailto: prefill)
        XCTAssertEqual(session.toText, "a@b.test")
        XCTAssertEqual(session.ccText, "c@b.test")
        XCTAssertEqual(session.bccText, "d@b.test")
        XCTAssertEqual(session.subject, "Hi")
        XCTAssertEqual(session.body, "Hello")
        // Untouched mailto: prefill is not itself a user edit — closing the
        // window without typing anything must not save a stray draft.
        XCTAssertFalse(session.hasUserEdits)
    }

    @MainActor
    func testSignatureIsInsertedAboveNoQuoteJustLikeANewMessage() {
        let prefill = MailtoPrefill(to: "a@b.test", body: "Hello")
        let session = ComposeSession(accountId: "acct1", mailto: prefill, signature: "Ada")
        XCTAssertTrue(session.body.contains("-- \nAda"))
        XCTAssertTrue(session.body.contains("Hello"))
    }
}

/// #3381: the Share Extension fills ordered slots on the main queue instead of appending to a
/// shared Array from concurrent loadItem callbacks.
final class ShareContentSlotsTests: XCTestCase {
    func testBodyKeepsReservationOrderWhateverTheCompletionOrder() {
        var slots = ShareContentSlots()
        let a = slots.reserve(), b = slots.reserve(), c = slots.reserve()
        slots.fill(c, with: "third")
        slots.fill(a, with: "first")
        slots.fill(b, with: "second")
        XCTAssertEqual(slots.body, "first\n\nsecond\n\nthird")
    }

    func testFailedOrEmptyItemsLeaveNoGap() {
        var slots = ShareContentSlots()
        let a = slots.reserve(), b = slots.reserve(), c = slots.reserve()
        slots.fill(a, with: "kept")
        slots.fill(b, with: nil)   // loadItem returned something unusable
        slots.fill(c, with: "")
        XCTAssertEqual(slots.body, "kept")
        slots.fill(99, with: "ignored")   // out of range is a no-op
        XCTAssertEqual(slots.body, "kept")
    }
}
