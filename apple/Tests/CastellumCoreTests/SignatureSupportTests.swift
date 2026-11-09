// Castellum - unit tests for the per-account signature pure functions (S26).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure-function coverage only: SignatureSupport never touches Preferences,
// UserDefaults, the Keychain or a profile directory, so none of this needs
// the AppModel/temp-profile-directory setup other integration tests use.

import XCTest
@testable import Castellum

final class SignatureSupportTests: XCTestCase {

    // MARK: - block(for:)

    func testEmptySignatureFormatsToAnEmptyBlock() {
        XCTAssertEqual(SignatureSupport.block(for: ""), "")
        XCTAssertEqual(SignatureSupport.block(for: "   \n  "), "")
    }

    func testSignatureBlockUsesTheStandardDelimiter() {
        XCTAssertEqual(SignatureSupport.block(for: "Ada Lovelace"), "-- \nAda Lovelace")
    }

    // MARK: - insert(signature:into:) — new message

    func testInsertIntoBlankNewMessagePutsSignatureBelowAnEmptyCaretLine() {
        let result = SignatureSupport.insert(signature: "Ada Lovelace", into: "")
        XCTAssertEqual(result, "\n\n-- \nAda Lovelace")
    }

    func testInsertWithEmptySignatureLeavesBodyUnchanged() {
        XCTAssertEqual(SignatureSupport.insert(signature: "", into: ""), "")
        XCTAssertEqual(SignatureSupport.insert(signature: "  ", into: "Hello"), "Hello")
        let replyBody = "\n\nAda wrote:\n> Original"
        XCTAssertEqual(SignatureSupport.insert(signature: "", into: replyBody), replyBody)
    }

    func testInsertIntoNonEmptyBodyWithNoSeparatorAppendsBelowABlankLine() {
        let result = SignatureSupport.insert(signature: "Ada", into: "Some existing text")
        XCTAssertEqual(result, "Some existing text\n\n-- \nAda")
    }

    // MARK: - insert(signature:into:) — reply, above the quote

    func testInsertIntoReplyPlacesSignatureAboveTheQuoteBlock() {
        let replyBody = ReplyPrefill.quotedReplyBody(
            original: "Sounds good.",
            senderDisplay: "Ada",
            sentAt: nil
        )
        XCTAssertEqual(replyBody, "\n\nAda wrote:\n> Sounds good.")

        let result = SignatureSupport.insert(signature: "Grace Hopper", into: replyBody)
        XCTAssertEqual(result, "\n\n-- \nGrace Hopper\n\nAda wrote:\n> Sounds good.")
    }

    // MARK: - hasUnmodifiedBlock

    func testHasUnmodifiedBlockDetectsAnUntouchedSignature() {
        let body = "\n\n-- \nAda Lovelace\n\nOn 1 Jan, Bob wrote:\n> hi"
        XCTAssertTrue(SignatureSupport.hasUnmodifiedBlock("Ada Lovelace", in: body))
    }

    func testHasUnmodifiedBlockIsFalseOnceTheUserEditsIt() {
        let body = "\n\n-- \nAda L. (edited)\n\nOn 1 Jan, Bob wrote:\n> hi"
        XCTAssertFalse(SignatureSupport.hasUnmodifiedBlock("Ada Lovelace", in: body))
    }

    func testHasUnmodifiedBlockIsFalseForAnEmptySignature() {
        XCTAssertFalse(SignatureSupport.hasUnmodifiedBlock("", in: "anything"))
    }

    // MARK: - swapForAccountChange

    func testSwapReplacesAnUntouchedSignatureBlock() {
        let body = SignatureSupport.insert(signature: "Ada Lovelace", into: "")
        let swapped = SignatureSupport.swapForAccountChange(
            from: "Ada Lovelace", to: "Grace Hopper", in: body
        )
        XCTAssertEqual(swapped, "\n\n-- \nGrace Hopper")
    }

    func testSwapLeavesAnEditedSignatureBlockUntouched() {
        var body = SignatureSupport.insert(signature: "Ada Lovelace", into: "")
        // The user edited the inserted signature block.
        body = body.replacingOccurrences(of: "Ada Lovelace", with: "Ada L. — Analytical Engine")
        let swapped = SignatureSupport.swapForAccountChange(
            from: "Ada Lovelace", to: "Grace Hopper", in: body
        )
        XCTAssertEqual(swapped, body, "an edited signature block must never be clobbered")
    }

    func testSwapLeavesARemovedSignatureBlockUntouched() {
        // Body has no trace of the old signature block (user deleted it).
        let body = "Just my own words."
        let swapped = SignatureSupport.swapForAccountChange(
            from: "Ada Lovelace", to: "Grace Hopper", in: body
        )
        XCTAssertEqual(swapped, body)
    }

    func testSwapFromNoSignatureInsertsAFreshOneForTheNewAccount() {
        // Original account had no signature configured, so nothing was
        // inserted; switching to an account that has one should add it.
        let body = "Draft in progress"
        let swapped = SignatureSupport.swapForAccountChange(from: "", to: "Grace Hopper", in: body)
        XCTAssertEqual(swapped, "Draft in progress\n\n-- \nGrace Hopper")
    }

    func testSwapToNoSignatureRemovesTheOldBlock() {
        let body = SignatureSupport.insert(signature: "Ada Lovelace", into: "")
        let swapped = SignatureSupport.swapForAccountChange(from: "Ada Lovelace", to: "", in: body)
        XCTAssertEqual(swapped, "\n\n")
    }

    // MARK: - swapForAccountChange — em-dash delimiter

    func testSwapFromNoSignatureDoesNotStackAnotherUnderAnEmDashDelimiter() {
        let body = "Hello\n\n\u{2014} \nAda Lovelace"
        let swapped = SignatureSupport.swapForAccountChange(from: "", to: "Grace Hopper", in: body)
        XCTAssertEqual(swapped, body, "a signature already present (em-dash delimiter) must not be re-added")
    }

    func testSwapFromNoSignatureDoesNotStackAnotherUnderAStandardDelimiter() {
        let body = "Hello\n\n-- \nAda Lovelace"
        XCTAssertEqual(SignatureSupport.swapForAccountChange(from: "", to: "Grace Hopper", in: body), body)
    }

    func testSwapReplacesAnUnmodifiedBlockWhoseDelimiterBecameAnEmDash() {
        let body = "Hello\n\n\u{2014} \nAda Lovelace"
        let swapped = SignatureSupport.swapForAccountChange(from: "Ada Lovelace", to: "Grace Hopper", in: body)
        XCTAssertEqual(swapped, "Hello\n\n-- \nGrace Hopper")
    }

    /// "\r\n" is ONE Character in Swift; a "\n" search never matched it.
    func testSwapReplacesAnUnmodifiedBlockInACRLFBody() {
        let body = "Hello\r\n\r\n\u{2014} \r\nAda Lovelace"
        let swapped = SignatureSupport.swapForAccountChange(from: "Ada Lovelace", to: "Grace Hopper", in: body)
        XCTAssertEqual(swapped, "Hello\r\n\r\n-- \nGrace Hopper")
    }

    func testSwapLeavesAnEditedSignatureInACRLFBodyUntouched() {
        let body = "Hello\r\n\r\n\u{2014} \r\nAda L."
        XCTAssertEqual(SignatureSupport.swapForAccountChange(from: "Ada Lovelace", to: "Grace Hopper", in: body), body)
    }

    func testSwapReplacesAnUnmodifiedBlockWithAnEnDashDelimiter() {
        let body = "Hello\n\n\u{2013} \nAda Lovelace"
        let swapped = SignatureSupport.swapForAccountChange(from: "Ada Lovelace", to: "Grace Hopper", in: body)
        XCTAssertEqual(swapped, "Hello\n\n-- \nGrace Hopper")
    }

    func testSwapReplacesAnUnmodifiedBlockWithADelimiterLackingTheTrailingSpace() {
        let body = "Hello\n\n--\nAda Lovelace"
        let swapped = SignatureSupport.swapForAccountChange(from: "Ada Lovelace", to: "Grace Hopper", in: body)
        XCTAssertEqual(swapped, "Hello\n\n-- \nGrace Hopper")
    }

    func testAnEmDashInsideTheBodyTextIsNotADelimiter() {
        let body = "Hello \u{2014} world"
        XCTAssertEqual(SignatureSupport.swapForAccountChange(from: "", to: "Grace Hopper", in: body),
                       "Hello \u{2014} world\n\n-- \nGrace Hopper")
    }

    // MARK: - swapForAccountChange — appended-to signature kept verbatim (bug fix)

    /// A substring search (the old implementation) would still find the old
    /// block as a PREFIX of the appended-to text and swap just that prefix,
    /// partially overwriting the user's own appended words. Structural
    /// comparison must see the whole region differs from the untouched
    /// block and leave it alone entirely.
    func testSwapLeavesASignatureThatWasOnlyAppendedToUntouched() {
        let body = SignatureSupport.insert(signature: "Ada Lovelace", into: "")
        let appended = body + "\nSent from my iPhone"
        let swapped = SignatureSupport.swapForAccountChange(
            from: "Ada Lovelace", to: "Grace Hopper", in: appended
        )
        XCTAssertEqual(swapped, appended,
                        "appending after the signature must never trigger a swap")
    }

    func testSwapLeavesASignatureAppendedToAboveAReplyQuoteUntouched() {
        let replyBody = ReplyPrefill.quotedReplyBody(
            original: "hi", senderDisplay: "Bob", sentAt: nil
        )
        let withSignature = SignatureSupport.insert(signature: "Ada Lovelace", into: replyBody)
        let appended = withSignature.replacingOccurrences(
            of: "Ada Lovelace\n\n", with: "Ada Lovelace\nSent from my iPhone\n\n"
        )
        let swapped = SignatureSupport.swapForAccountChange(
            from: "Ada Lovelace", to: "Grace Hopper", in: appended
        )
        XCTAssertEqual(swapped, appended,
                        "appending after the signature (even above a reply quote) must not swap")
    }

    // MARK: - swapForAccountChange — no-signature-to-signature placement (bug fix)

    /// A naive first-"\n\n" insertion point (the old `insert` behaviour) would
    /// land the fresh signature between the user's first two paragraphs
    /// instead of at the end of their text.
    func testSwapFromNoSignaturePlacesTheNewSignatureAtTheEndForANewMessage() {
        let body = "Hi Bob,\n\nThanks for reaching out.\n\nBest,"
        let swapped = SignatureSupport.swapForAccountChange(from: "", to: "Grace Hopper", in: body)
        XCTAssertEqual(swapped, "Hi Bob,\n\nThanks for reaching out.\n\nBest,\n\n-- \nGrace Hopper")
    }

    /// Same hazard for a reply: several paragraphs of the user's own text
    /// above the quote must not get the signature wedged between them — it
    /// belongs directly above the quote, exactly where a fresh insert puts it.
    func testSwapFromNoSignaturePlacesTheNewSignatureAboveTheQuoteForAReply() {
        let body = "Thanks!\n\nSee you then.\n\nAda wrote:\n> Original message"
        let swapped = SignatureSupport.swapForAccountChange(from: "", to: "Grace Hopper", in: body)
        XCTAssertEqual(
            swapped,
            "Thanks!\n\nSee you then.\n\n-- \nGrace Hopper\n\nAda wrote:\n> Original message"
        )
    }
}

// MARK: - ComposeSession wiring (S26)

@MainActor
final class ComposeSessionSignatureTests: XCTestCase {
    func testNewMessageSessionInsertsTheAccountSignatureAndIsNotAnEdit() {
        let s = ComposeSession(accountId: "acct", signature: "Ada Lovelace")
        XCTAssertEqual(s.body, "\n\n-- \nAda Lovelace")
        // Generated content, exactly like a reply's quoted block — not a
        // user edit, so an unmodified window still closes as a no-op.
        XCTAssertFalse(s.hasUserEdits)
        XCTAssertEqual(s.closeAction, .none)
    }

    func testReplySessionInsertsSignatureAboveTheQuote() {
        let prefill = ReplyPrefill(
            to: [DraftAddress(name: "Ada", email: "ada@x.test")],
            subject: "Re: Plans",
            inReplyTo: "<orig@x.test>",
            referencesHeader: "<orig@x.test>",
            body: "\n\nAda wrote:\n> Original"
        )
        let s = ComposeSession(accountId: "acct", prefill: prefill, signature: "Grace Hopper")
        XCTAssertEqual(s.body, "\n\n-- \nGrace Hopper\n\nAda wrote:\n> Original")
        XCTAssertFalse(s.hasUserEdits)
    }

    func testEmptySignatureLeavesANewSessionBodyUntouched() {
        let s = ComposeSession(accountId: "acct", signature: "")
        XCTAssertEqual(s.body, "")
        XCTAssertFalse(s.hasContent)
    }

    /// Reopening a saved draft must never insert a signature a second time —
    /// the draft's own body (whatever it holds, signature included or not)
    /// is used verbatim.
    func testReopeningADraftDoesNotInsertTheSignatureAgain() {
        let stored = Draft(
            id: 3, accountId: "acct",
            to: [DraftAddress(name: nil, email: "ada@x.test")],
            cc: [], bcc: [], subject: "Hi",
            bodyText: "\n\n-- \nAda Lovelace", bodyHtml: nil,
            fromEmail: nil, fromName: nil,
            inReplyTo: nil, referencesHeader: nil,
            status: "draft", updatedAtEpoch: 0,
            serverMailboxPath: nil, serverUid: nil, serverUidValidity: nil,
            serverMessageId: nil, serverSavedAtEpoch: nil
        )
        let s = ComposeSession(draft: stored)
        XCTAssertEqual(s.body, "\n\n-- \nAda Lovelace")
        XCTAssertFalse(s.hasUserEdits)
    }

    func testChangingAccountSwapsAnUnmodifiedSignature() {
        // Isolated UserDefaults suite — never touches the app's real
        // Preferences.shared / UserDefaults domain.
        let defaults = UserDefaults(suiteName: "signature-swap-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        prefs.setSignature("Ada Lovelace", for: "acctA")
        prefs.setSignature("Grace Hopper", for: "acctB")

        let s = ComposeSession(accountId: "acctA", signature: prefs.signature(for: "acctA"),
                               preferences: prefs)
        XCTAssertEqual(s.body, "\n\n-- \nAda Lovelace")

        s.accountId = "acctB"
        XCTAssertEqual(s.body, "\n\n-- \nGrace Hopper")
    }

    func testChangingAccountLeavesAnEditedSignatureUntouched() {
        let defaults = UserDefaults(suiteName: "signature-swap-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        prefs.setSignature("Ada Lovelace", for: "acctA")
        prefs.setSignature("Grace Hopper", for: "acctB")

        let s = ComposeSession(accountId: "acctA", signature: prefs.signature(for: "acctA"),
                               preferences: prefs)
        s.body = "Thanks!" + s.body.replacingOccurrences(of: "Ada Lovelace", with: "Ada L.")

        s.accountId = "acctB"
        XCTAssertTrue(s.body.contains("Ada L."), "an edited signature must survive an account switch")
        XCTAssertFalse(s.body.contains("Grace Hopper"))
    }
}
