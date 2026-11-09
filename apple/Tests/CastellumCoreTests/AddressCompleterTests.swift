// Castellum - Tests for S47's pure address-autocomplete ranking.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AddressCompleterTests: XCTestCase {
    private func contact(
        _ email: String, name: String? = nil, lastSeenAt: Int64? = 1000,
        inbound: Int64 = 1, outbound: Int64 = 0
    ) -> Contact {
        Contact(id: 1, email: email, derivedName: name, displayName: nil, notes: "",
                isHidden: false, isSelf: false, isUserCreated: false,
                firstSeenAt: nil, lastSeenAt: lastSeenAt, inboundCount: inbound, outboundCount: outbound)
    }

    func testEmptyPrefixReturnsNoSuggestions() {
        let result = AddressCompleter.suggestions(
            for: "", localContacts: [contact("a@x.test")], systemContacts: [])
        XCTAssertTrue(result.isEmpty)
    }

    func testMatchesOnEmailSubstring() {
        let result = AddressCompleter.suggestions(
            for: "example.com", localContacts: [contact("bob@example.com")], systemContacts: [])
        XCTAssertEqual(result.map(\.email), ["bob@example.com"])
    }

    func testMatchesOnNameSubstringNotJustPrefix() {
        let result = AddressCompleter.suggestions(
            for: "smith", localContacts: [contact("j@x.test", name: "John Smith")], systemContacts: [])
        XCTAssertEqual(result.map(\.email), ["j@x.test"])
    }

    func testLocalContactsRankBeforeSystemOnlyContacts() {
        let result = AddressCompleter.suggestions(
            for: "x",
            localContacts: [contact("z@x.test")],
            systemContacts: [SystemContactEntry(name: nil, email: "a@x.test")])
        XCTAssertEqual(result.map(\.email), ["z@x.test", "a@x.test"], "local, even alphabetically later, wins")
    }

    func testDeduplicatesByEmailPreferringTheLocalName() {
        let result = AddressCompleter.suggestions(
            for: "x",
            localContacts: [contact("a@x.test", name: "Mail-Derived Name")],
            systemContacts: [SystemContactEntry(name: "Address Book Name", email: "a@x.test")])
        XCTAssertEqual(result.count, 1)
        XCTAssertEqual(result[0].name, "Mail-Derived Name")
    }

    func testWithinLocalContactsRanksByRecencyThenFrequency() {
        let older = contact("old@x.test", lastSeenAt: 100)
        let newer = contact("new@x.test", lastSeenAt: 200)
        let result = AddressCompleter.suggestions(for: "x.test", localContacts: [older, newer], systemContacts: [])
        XCTAssertEqual(result.map(\.email), ["new@x.test", "old@x.test"])

        let frequent = contact("freq@x.test", lastSeenAt: 100, inbound: 10, outbound: 5)
        let rare = contact("rare@x.test", lastSeenAt: 100, inbound: 1, outbound: 0)
        let byFrequency = AddressCompleter.suggestions(for: "x.test", localContacts: [rare, frequent], systemContacts: [])
        XCTAssertEqual(byFrequency.map(\.email), ["freq@x.test", "rare@x.test"], "same recency, frequency breaks the tie")
    }

    func testLimitIsEnforced() {
        let many = (0..<20).map { contact("n\($0)@x.test") }
        let result = AddressCompleter.suggestions(for: "x.test", localContacts: many, systemContacts: [], limit: 5)
        XCTAssertEqual(result.count, 5)
    }

    func testCompletionTextFormatsNameAndEmailOrBareEmail() {
        XCTAssertEqual(AddressSuggestion(name: "Ada Lovelace", email: "ada@x.test").completionText, "Ada Lovelace <ada@x.test>")
        XCTAssertEqual(AddressSuggestion(name: nil, email: "ada@x.test").completionText, "ada@x.test")
        XCTAssertEqual(AddressSuggestion(name: "", email: "ada@x.test").completionText, "ada@x.test")
    }

    // MARK: - recipients of sent mail (#3147)

    private func sent(_ email: String, name: String? = nil, at: Int64 = 2000, count: Int64 = 1) -> SentRecipient {
        SentRecipient(email: email, name: name, lastSentAt: at, sentCount: count)
    }

    func testSentRecipientIsSuggestedWithoutAnyContactRow() {
        let result = AddressCompleter.suggestions(
            for: "alice", localContacts: [], systemContacts: [],
            sentRecipients: [sent("alice@local.test", name: "Alice")])
        XCTAssertEqual(result.map(\.completionText), ["Alice <alice@local.test>"])
    }

    func testSentRecipientMergesWithContactByEmailIgnoringCase() {
        let result = AddressCompleter.suggestions(
            for: "bob", localContacts: [contact("bob@x.test", name: "Bob Derived")],
            systemContacts: [], sentRecipients: [sent("BOB@x.test", name: "Other")])
        XCTAssertEqual(result.count, 1)
        XCTAssertEqual(result[0].name, "Bob Derived", "contact name wins")
    }

    func testRecentlySentOutranksOlderInboundOnly() {
        let result = AddressCompleter.suggestions(
            for: "x.test",
            localContacts: [contact("old@x.test", lastSeenAt: 1000, inbound: 50)],
            systemContacts: [], sentRecipients: [sent("new@x.test", at: 3000)])
        XCTAssertEqual(result.map(\.email), ["new@x.test", "old@x.test"])
    }

    func testSentWeightBreaksARecencyTie() {
        let result = AddressCompleter.suggestions(
            for: "x.test",
            localContacts: [contact("in@x.test", lastSeenAt: 2000, inbound: 3)],
            systemContacts: [], sentRecipients: [sent("out@x.test", at: 2000, count: 2)])
        XCTAssertEqual(result.map(\.email), ["out@x.test", "in@x.test"], "sent counts double")
    }

    func testLimitStillApplies() {
        let many = (0..<20).map { sent("p\($0)@x.test", at: Int64($0)) }
        let result = AddressCompleter.suggestions(
            for: "x.test", localContacts: [], systemContacts: [], sentRecipients: many)
        XCTAssertEqual(result.count, 8)
        XCTAssertEqual(result.first?.email, "p19@x.test")
    }

    // MARK: - recipient injection (security review of #231)

    func testHostileNamesRoundTripToExactlyOneRecipient() {
        let names = [
            "Bob, evil@x.com", "Bob; evil@x.com", "Bob \"x\" <evil@x.com>",
            "Bob\"> , evil@x.com <", "Bob\r\nBcc: evil@x.com", "a\\\", evil@x.com",
            "Doe, John (Acme)", "x@y.com",
        ]
        for name in names {
            let text = AddressSuggestion(name: name, email: "bob@good.test").completionText
            let parsed = AddressParser.parse(text)
            XCTAssertEqual(parsed.addresses.count, 1, "name \(name.debugDescription) -> \(text.debugDescription)")
            XCTAssertTrue(parsed.invalid.isEmpty, "\(text.debugDescription)")
            XCTAssertEqual(parsed.addresses.first?.email, "bob@good.test")
            XCTAssertFalse(text.contains("\n") || text.contains("\r"))
        }
    }

    func testPlainAndCommaNamesKeepTheirDisplayName() {
        XCTAssertEqual(AddressSuggestion(name: "Alice", email: "a@x.test").completionText, "Alice <a@x.test>")
        let parsed = AddressParser.parse(AddressSuggestion(name: "Doe, John", email: "j@x.test").completionText)
        XCTAssertEqual(parsed.addresses, [DraftAddress(name: "Doe, John", email: "j@x.test")])
    }

    func testUnsafeAddressesAreNeverSuggested() {
        let result = AddressCompleter.suggestions(
            for: "x", localContacts: [contact("a@x.test>, evil@x.com"), contact("ok@x.test")],
            systemContacts: [SystemContactEntry(name: nil, email: "b c@x.test")],
            sentRecipients: [sent("d@x.test,e@x.test")])
        XCTAssertEqual(result.map(\.email), ["ok@x.test"])
    }
}
