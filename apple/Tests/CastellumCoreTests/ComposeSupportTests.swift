// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

// MARK: - AddressParser

final class AddressParserTests: XCTestCase {
    func testBareEmail() {
        let r = AddressParser.parse("a@b.test")
        XCTAssertEqual(r.addresses, [DraftAddress(name: nil, email: "a@b.test")])
        XCTAssertTrue(r.invalid.isEmpty)
    }

    func testNameAngleForm() {
        let r = AddressParser.parse("Ada Lovelace <ada@calc.test>")
        XCTAssertEqual(r.addresses, [DraftAddress(name: "Ada Lovelace", email: "ada@calc.test")])
    }

    func testQuotedNameWithComma() {
        let r = AddressParser.parse(#""Lovelace, Ada" <ada@calc.test>, bob@x.test"#)
        XCTAssertEqual(r.addresses, [
            DraftAddress(name: "Lovelace, Ada", email: "ada@calc.test"),
            DraftAddress(name: nil, email: "bob@x.test"),
        ])
        XCTAssertTrue(r.invalid.isEmpty)
    }

    func testQuotedNameWithEscapedQuote() {
        let r = AddressParser.parse(#""Ada \"the analyst\"" <ada@calc.test>"#)
        XCTAssertEqual(r.addresses, [DraftAddress(name: #"Ada "the analyst""#, email: "ada@calc.test")])
    }

    func testSemicolonAndCommaSeparators() {
        let r = AddressParser.parse("a@x.test; b@x.test , C <c@x.test>")
        XCTAssertEqual(r.addresses.map(\.email), ["a@x.test", "b@x.test", "c@x.test"])
        XCTAssertEqual(r.addresses[2].name, "C")
    }

    func testInvalidEntriesAreReported() {
        let r = AddressParser.parse("not-an-email, a@b.test, whoops <also bad>, @missing.local, x@nodot")
        XCTAssertEqual(r.addresses.map(\.email), ["a@b.test"])
        XCTAssertEqual(r.invalid, ["not-an-email", "whoops <also bad>", "@missing.local", "x@nodot"])
    }

    func testEmptyAndWhitespaceInput() {
        XCTAssertEqual(AddressParser.parse(""), AddressParser.ParseResult())
        XCTAssertEqual(AddressParser.parse("  , ;  "), AddressParser.ParseResult())
    }

    func testWhitespaceAroundEntriesIsTrimmed() {
        let r = AddressParser.parse("  a@b.test ,  Bob <bob@b.test>  ")
        XCTAssertEqual(r.addresses, [
            DraftAddress(name: nil, email: "a@b.test"),
            DraftAddress(name: "Bob", email: "bob@b.test"),
        ])
    }
}

// MARK: - AccountConfig / SMTP defaulting

final class AccountConfigTests: XCTestCase {
    /// Exactly what pre-compose AccountSetupSheet versions wrote to the Keychain.
    private let oldConfigJSON = """
    {"account_id":"a@x.test","email":"a@x.test","host":"imap.x.test","port":993,\
    "username":"a@x.test","password":"s3cret","use_tls":true,"allow_invalid_certs":false}
    """

    func testDecodesConfigSavedWithoutSMTPFields() throws {
        let cfg = try AccountConfig.decode(fromJSON: oldConfigJSON)
        XCTAssertEqual(cfg.accountId, "a@x.test")
        XCTAssertEqual(cfg.host, "imap.x.test")
        XCTAssertNil(cfg.smtpHost)
        XCTAssertNil(cfg.smtpPort)
    }

    func testEffectiveSMTPDefaultsFromIMAPValues() throws {
        let smtp = try AccountConfig.decode(fromJSON: oldConfigJSON).effectiveSMTP
        XCTAssertEqual(smtp, SMTPSettings(host: "smtp.x.test", port: 587,
                                          username: "a@x.test", password: "s3cret",
                                          useTls: true))
    }

    func testDefaultSMTPHostMapsImapPrefixElseSameHost() {
        XCTAssertEqual(AccountConfig.defaultSMTPHost(imapHost: "imap.example.com"), "smtp.example.com")
        XCTAssertEqual(AccountConfig.defaultSMTPHost(imapHost: "IMAP.example.com"), "smtp.example.com")
        XCTAssertEqual(AccountConfig.defaultSMTPHost(imapHost: "mail.example.com"), "mail.example.com")
        XCTAssertEqual(AccountConfig.defaultSMTPHost(imapHost: "localhost"), "localhost")
    }

    func testExplicitSMTPFieldsWinOverDefaults() throws {
        var cfg = try AccountConfig.decode(fromJSON: oldConfigJSON)
        cfg.smtpHost = "send.x.test"
        cfg.smtpPort = 2525
        cfg.smtpUsername = "sender"
        cfg.smtpPassword = "other"
        cfg.smtpUseTls = false
        XCTAssertEqual(cfg.effectiveSMTP, SMTPSettings(host: "send.x.test", port: 2525,
                                                       username: "sender", password: "other",
                                                       useTls: false))
    }

    func testEmptySMTPStringsFallBackToDefaults() throws {
        var cfg = try AccountConfig.decode(fromJSON: oldConfigJSON)
        cfg.smtpHost = ""
        cfg.smtpUsername = ""
        cfg.smtpPassword = ""
        let smtp = cfg.effectiveSMTP
        XCTAssertEqual(smtp.host, "smtp.x.test")
        XCTAssertEqual(smtp.username, "a@x.test")
        XCTAssertEqual(smtp.password, "s3cret")
    }

    func testEncodedJSONRoundTripsIncludingSMTPFields() throws {
        var cfg = try AccountConfig.decode(fromJSON: oldConfigJSON)
        cfg.smtpHost = "send.x.test"
        cfg.smtpPort = 465
        let json = try cfg.encodedJSON()
        XCTAssertEqual(try AccountConfig.decode(fromJSON: json), cfg)
        // Keys must be snake_case so the Rust sync deserializer keeps working.
        XCTAssertTrue(json.contains("\"account_id\""))
        XCTAssertTrue(json.contains("\"use_tls\""))
        XCTAssertTrue(json.contains("\"smtp_host\""))
    }

    func testSendConfigJSONShape() throws {
        let cfg = try AccountConfig.decode(fromJSON: oldConfigJSON)
        let json = try cfg.sendConfigJSON()
        let obj = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        XCTAssertEqual(obj["account_id"] as? String, "a@x.test")
        XCTAssertEqual(obj["from_email"] as? String, "a@x.test")
        let smtp = try XCTUnwrap(obj["smtp"] as? [String: Any])
        XCTAssertEqual(smtp["host"] as? String, "smtp.x.test")
        XCTAssertEqual(smtp["port"] as? Int, 587)
        XCTAssertEqual(smtp["username"] as? String, "a@x.test")
        XCTAssertEqual(smtp["password"] as? String, "s3cret")
        XCTAssertEqual(smtp["use_tls"] as? Bool, true)
        XCTAssertEqual(smtp["allow_invalid_certs"] as? Bool, false)
        // Old configs have no display name -> from_name key entirely absent
        // (Rust SendConfig.from_name is #[serde(default)] Option).
        XCTAssertNil(obj["from_name"])
    }

    /// MM-17 S2: the format=flowed switch is written as `plain_text_flowed`
    /// (the core's SendConfig key); the parameter defaults to false.
    func testSendConfigJSONWritesThePlainTextFlowedFlag() throws {
        let cfg = try AccountConfig.decode(fromJSON: oldConfigJSON)
        func flag(_ json: String) throws -> Bool? {
            let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
            return obj["plain_text_flowed"] as? Bool
        }
        XCTAssertEqual(try flag(cfg.sendConfigJSON()), false)
        XCTAssertEqual(try flag(cfg.sendConfigJSON(plainTextFlowed: true)), true)
        XCTAssertEqual(try flag(cfg.sendConfigJSON(plainTextFlowed: false)), false)
    }

    /// The Settings > General toggle: default ON (decision M1), persisted.
    @MainActor
    func testSendFlowedPreferenceDefaultsOnAndPersists() {
        let suite = "sendflowed-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let prefs = Preferences(defaults: defaults)
        XCTAssertTrue(prefs.sendFlowed)
        prefs.sendFlowed = false
        XCTAssertFalse(Preferences(defaults: defaults).sendFlowed)
    }

    func testSendConfigJSONCarriesDisplayNameAsFromName() throws {
        var cfg = try AccountConfig.decode(fromJSON: oldConfigJSON)
        cfg.displayName = "Ada Lovelace"
        let obj = try XCTUnwrap(
            JSONSerialization.jsonObject(
                with: Data(cfg.sendConfigJSON().utf8)) as? [String: Any])
        XCTAssertEqual(obj["from_name"] as? String, "Ada Lovelace")

        // Empty display name behaves like nil (no bare "" From name).
        cfg.displayName = ""
        let obj2 = try XCTUnwrap(
            JSONSerialization.jsonObject(
                with: Data(cfg.sendConfigJSON().utf8)) as? [String: Any])
        XCTAssertNil(obj2["from_name"])
    }

    func testDisplayNameRoundTripsAndOldConfigsDecodeWithoutIt() throws {
        let old = try AccountConfig.decode(fromJSON: oldConfigJSON)
        XCTAssertNil(old.displayName)

        var cfg = old
        cfg.displayName = "Ada Lovelace"
        let decoded = try AccountConfig.decode(fromJSON: cfg.encodedJSON())
        XCTAssertEqual(decoded.displayName, "Ada Lovelace")
    }
}

// MARK: - InsertAboveQuote

final class InsertAboveQuoteTests: XCTestCase {
    // Convenience: call the static method under test.
    private func insert(_ suggestion: String, into body: String) -> String {
        ReplyPrefill.insertAboveQuote(suggestion: suggestion, in: body)
    }

    func testStandardReplyBodyInsertsAboveAttribution() {
        // The typical reply body from quotedReplyBody starts with "\n\n".
        let body = "\n\nOn Jul 3, Ada wrote:\n> Hello"
        let result = insert("Accept, free Thu", into: body)
        XCTAssertEqual(result, "Accept, free Thu\n\nOn Jul 3, Ada wrote:\n> Hello")
    }

    func testEmptyBodyBecomesJustSuggestion() {
        XCTAssertEqual(insert("Sounds good", into: ""), "Sounds good")
    }

    func testWhitespaceOnlyBodyBecomesJustSuggestion() {
        // Blank reply body (no original text) should work the same as empty.
        XCTAssertEqual(insert("Draft text", into: "   \n  "), "Draft text")
    }

    func testUserAlreadyTypedAboveQuoteIsReplacedBySuggestion() {
        // If the user typed text before the "\n\n{attribution}" separator,
        // "insert at top, above the quoted block" replaces their preamble.
        let body = "Hey,\n\nOn Jul 3, Ada wrote:\n> Hello"
        XCTAssertEqual(
            insert("AI text", into: body),
            "AI text\n\nOn Jul 3, Ada wrote:\n> Hello"
        )
    }

    func testBodyWithoutQuoteSeparatorPrependsWithNewlines() {
        // A new-message body with content but no attribution separator.
        let result = insert("Opening line", into: "Some ideas")
        XCTAssertEqual(result, "Opening line\n\nSome ideas")
    }

    func testQuoteBlockIsPreservedVerbatim() {
        // Attribution + multi-line quote should survive unchanged.
        let body = "\n\nAda wrote:\n> line one\n>> nested quote\n> line two"
        let result = insert("Thanks!", into: body)
        XCTAssertEqual(result, "Thanks!\n\nAda wrote:\n> line one\n>> nested quote\n> line two")
    }

    func testSuggestionCanBeMultiline() {
        let suggestion = "Hi Ada,\n\nThursday works for me.\n\nBest"
        let body = "\n\nOn Jul 3, Ada wrote:\n> Can we meet?"
        let result = insert(suggestion, into: body)
        XCTAssertTrue(result.hasPrefix("Hi Ada,\n\nThursday works for me."))
        XCTAssertTrue(result.hasSuffix("On Jul 3, Ada wrote:\n> Can we meet?"))
    }
}

// MARK: - ReplyPrefill

final class ReplyPrefillTests: XCTestCase {
    private func detail(subject: String?,
                        addresses: [Address],
                        messageId: String? = "<orig-1@x.test>",
                        textBody: String? = "hello",
                        sentAt: Int64? = nil) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb",
                subject: subject, fromName: "Ada", fromAddress: "ada@x.test",
                sentAt: sentAt, snippet: nil, hasAttachments: false,
                hasBody: true,
                isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: messageId,
            addresses: addresses,
            textBody: textBody, sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
    }

    private let fullAddresses: [Address] = [
        Address(kind: "from", name: "Ada", address: "ada@x.test"),
        Address(kind: "to", name: "Me", address: "me@x.test"),
        Address(kind: "to", name: "Bob", address: "bob@x.test"),
        Address(kind: "cc", name: nil, address: "carol@x.test"),
        Address(kind: "cc", name: "Ada dup", address: "ADA@x.test"), // dup of sender
    ]

    // MARK: References chain (#3443)

    func testReplyReferencesCarriesTheParentsChainThenTheParentsId() {
        var d = detail(subject: "Plans", addresses: fullAddresses)
        d = MessageDetail(
            header: d.header, rfc822MessageId: "<orig-1@x.test>", addresses: d.addresses,
            textBody: d.textBody, sanitizedHtml: nil, references: ["<root@x>", "<mid@x>"])
        let p = ReplyPrefill.make(detail: d, mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(p.inReplyTo, "<orig-1@x.test>")
        XCTAssertEqual(p.referencesHeader, "<root@x> <mid@x> <orig-1@x.test>")
    }

    func testReferencesChainDeduplicatesAndTrimsVeryLongChains() {
        XCTAssertEqual(
            ReplyPrefill.referencesChain(parentReferences: ["<a>", "<b>", "<a>"], parentMessageId: "<b>"),
            "<a> <b>")
        XCTAssertNil(ReplyPrefill.referencesChain(parentReferences: [], parentMessageId: nil))
        XCTAssertEqual(ReplyPrefill.referencesChain(parentReferences: [], parentMessageId: "<p>"), "<p>")
        let long = (1...30).map { "<m\($0)>" }
        let chain = ReplyPrefill.referencesChain(parentReferences: long, parentMessageId: "<parent>")
        let ids = chain?.split(separator: " ").map(String.init) ?? []
        XCTAssertEqual(ids.count, 20)
        XCTAssertEqual(ids.first, "<m1>", "the root stays")
        XCTAssertEqual(ids.last, "<parent>", "the parent stays")
    }

    func testReplyTargetsOriginalFromOnly() {
        let p = ReplyPrefill.make(detail: detail(subject: "Plans", addresses: fullAddresses),
                                  mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(p.to, [DraftAddress(name: "Ada", email: "ada@x.test")])
        XCTAssertEqual(p.subject, "Re: Plans")
        XCTAssertEqual(p.inReplyTo, "<orig-1@x.test>")
        XCTAssertEqual(p.referencesHeader, "<orig-1@x.test>")
    }

    func testReplyAllKeepsToOnToAndCcOnCcMinusSelfDeduplicated() {
        let p = ReplyPrefill.make(detail: detail(subject: "Plans", addresses: fullAddresses),
                                  mode: .replyAll, selfEmail: "ME@x.test".lowercased())
        XCTAssertEqual(p.to.map(\.email), ["ada@x.test", "bob@x.test"])
        XCTAssertEqual(p.cc.map(\.email), ["carol@x.test"], "original Cc stays on Cc (#3443)")
        XCTAssertEqual(p.ccText, "carol@x.test")
    }

    func testReplyAllAndReplyDropTheUsersAliases() {
        let addrs: [Address] = [
            Address(kind: "from", name: "Ada", address: "ada@x.test"),
            Address(kind: "to", name: "Me", address: "me@x.test"),
            Address(kind: "to", name: "Me alias", address: "Alias@x.test"),
            Address(kind: "cc", name: nil, address: "other-alias@x.test"),
            Address(kind: "cc", name: nil, address: "dan@x.test"),
        ]
        let all = ReplyPrefill.make(detail: detail(subject: "S", addresses: addrs), mode: .replyAll,
                                    selfEmail: "me@x.test", aliases: ["alias@x.test", "other-alias@x.test"])
        XCTAssertEqual(all.to.map(\.email), ["ada@x.test"])
        XCTAssertEqual(all.cc.map(\.email), ["dan@x.test"])
        // Replying to a message I sent from an alias answers the people I wrote to.
        let sentFromAlias: [Address] = [
            Address(kind: "from", name: "Me", address: "alias@x.test"),
            Address(kind: "to", name: "Bob", address: "bob@x.test"),
        ]
        let r = ReplyPrefill.make(detail: detail(subject: "S", addresses: sentFromAlias), mode: .reply,
                                  selfEmail: "me@x.test", aliases: ["alias@x.test"])
        XCTAssertEqual(r.to.map(\.email), ["bob@x.test"])
    }

    func testRecipientsWithQuotesAndCommasRoundTrip() {
        let tricky = [
            DraftAddress(name: "Doe, John \"Johnny\"", email: "j@x.test"),
            DraftAddress(name: "Back\\slash, Inc", email: "b@x.test"),
            DraftAddress(name: nil, email: "plain@x.test"),
        ]
        let text = AddressFormatter.text(tricky)
        let parsed = AddressParser.parse(text)
        XCTAssertTrue(parsed.invalid.isEmpty, "\(parsed.invalid)")
        XCTAssertEqual(parsed.addresses, tricky)
    }

    func testAnOddNumberOfQuotesDoesNotBlockTheOtherRecipients() {
        let parsed = AddressParser.parse("Bob \"The Builder <bob@x.test>, carol@x.test, Dan <dan@x.test>")
        XCTAssertTrue(parsed.invalid.isEmpty, "\(parsed.invalid)")
        XCTAssertEqual(parsed.addresses.map(\.email), ["bob@x.test", "carol@x.test", "dan@x.test"])
    }

    func testReplyAllToOwnMessageFallsBackToSender() {
        let own: [Address] = [
            Address(kind: "from", name: "Me", address: "me@x.test"),
            Address(kind: "to", name: "Me", address: "me@x.test"),
        ]
        let p = ReplyPrefill.make(detail: detail(subject: "Note to self", addresses: own),
                                  mode: .replyAll, selfEmail: "me@x.test")
        XCTAssertEqual(p.to.map(\.email), ["me@x.test"])
    }

    func testReplyHonoursReplyToHeader() {
        let addrs = fullAddresses + [Address(kind: "reply-to", name: "List", address: "list@x.test")]
        let r = ReplyPrefill.make(detail: detail(subject: "Plans", addresses: addrs),
                                  mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(r.to, [DraftAddress(name: "List", email: "list@x.test")])
        let all = ReplyPrefill.make(detail: detail(subject: "Plans", addresses: addrs),
                                    mode: .replyAll, selfEmail: "me@x.test")
        XCTAssertEqual(all.to.map { $0.email.lowercased() },
                       ["list@x.test", "ada@x.test", "bob@x.test"])
        XCTAssertEqual(all.cc.map { $0.email.lowercased() }, ["carol@x.test"])
        // Reply-all keeps the visible original sender even when it appears
        // nowhere but From (no "Ada dup" Cc to rescue it).
        let noDup = addrs.filter { $0.name != "Ada dup" }
        let allNoDup = ReplyPrefill.make(detail: detail(subject: "Plans", addresses: noDup),
                                         mode: .replyAll, selfEmail: "me@x.test")
        XCTAssertEqual(allNoDup.to.map(\.email), ["list@x.test", "ada@x.test", "bob@x.test"])
        XCTAssertEqual(allNoDup.cc.map(\.email), ["carol@x.test"])
        // No Reply-To: unchanged.
        let plain = ReplyPrefill.make(detail: detail(subject: "Plans", addresses: fullAddresses),
                                      mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(plain.to.map(\.email), ["ada@x.test"])
    }

    func testReplyToOwnMessageTargetsOriginalRecipients() {
        let sent: [Address] = [
            Address(kind: "from", name: "Me", address: "me@x.test"),
            Address(kind: "to", name: "Bob", address: "bob@x.test"),
            Address(kind: "to", name: "Carol", address: "carol@x.test"),
        ]
        let r = ReplyPrefill.make(detail: detail(subject: "Hi", addresses: sent),
                                  mode: .reply, selfEmail: "ME@x.test")
        XCTAssertEqual(r.to.map(\.email), ["bob@x.test", "carol@x.test"])
        let note: [Address] = [
            Address(kind: "from", name: "Me", address: "me@x.test"),
            Address(kind: "to", name: "Me", address: "me@x.test"),
        ]
        let n = ReplyPrefill.make(detail: detail(subject: "Note", addresses: note),
                                  mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(n.to.map(\.email), ["me@x.test"])
    }

    func testSubjectReIsNotDoubled() {
        for existing in ["Re: Plans", "re: Plans", "RE: Plans"] {
            let p = ReplyPrefill.make(detail: detail(subject: existing, addresses: fullAddresses),
                                      mode: .reply, selfEmail: "me@x.test")
            XCTAssertEqual(p.subject, existing)
        }
    }

    func testNilSubjectAndMessageId() {
        let p = ReplyPrefill.make(detail: detail(subject: nil, addresses: fullAddresses, messageId: nil),
                                  mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(p.subject, "Re: ")
        XCTAssertNil(p.inReplyTo)
        XCTAssertNil(p.referencesHeader)
    }

    func testFallsBackToHeaderFromWhenAddressesLackFrom() {
        let p = ReplyPrefill.make(detail: detail(subject: "x", addresses: []),
                                  mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(p.to, [DraftAddress(name: "Ada", email: "ada@x.test")])
    }

    func testToTextQuotesNamesWithSeparatorsAndRoundTrips() {
        let p = ReplyPrefill(to: [DraftAddress(name: "Lovelace, Ada", email: "ada@x.test"),
                                  DraftAddress(name: nil, email: "bob@x.test")],
                             subject: "Re: x", inReplyTo: nil, referencesHeader: nil)
        XCTAssertEqual(p.toText, #""Lovelace, Ada" <ada@x.test>, bob@x.test"#)
        XCTAssertEqual(AddressParser.parse(p.toText).addresses, p.to)
    }
}

// MARK: - reply body quoting

final class ReplyQuotingTests: XCTestCase {
    private func attributionDate(_ epoch: Int64) -> String {
        ReplyPrefill.attributionDateFormatter
            .string(from: Date(timeIntervalSince1970: TimeInterval(epoch)))
    }

    func testAttributionWithDateAndQuotedLines() {
        let body = ReplyPrefill.quotedReplyBody(
            original: "line one\nline two",
            senderDisplay: "Ada", sentAt: 1_750_000_000)
        let date = attributionDate(1_750_000_000)
        XCTAssertEqual(body, "\n\nOn \(date), Ada wrote:\n> line one\n> line two")
    }

    func testCursorSitsAtTheTop() {
        // Superhuman style: two newlines above the attribution so the user
        // types at the top of the body.
        let body = ReplyPrefill.quotedReplyBody(original: "x", senderDisplay: "Ada", sentAt: nil)
        XCTAssertTrue(body.hasPrefix("\n\n"))
        XCTAssertFalse(body.hasPrefix("\n\n\n"), "exactly two leading newlines")
    }

    func testNilAndBlankOriginalsProduceNoBlock() {
        for original in [nil, "", "   ", "\n\n \n"] as [String?] {
            XCTAssertEqual(
                ReplyPrefill.quotedReplyBody(original: original, senderDisplay: "Ada",
                                             sentAt: 1_750_000_000),
                "", "no attribution over an empty body (original: \(String(describing: original)))")
        }
    }

    func testMissingDateOmitsOnClause() {
        let body = ReplyPrefill.quotedReplyBody(original: "hello", senderDisplay: "Ada", sentAt: nil)
        XCTAssertEqual(body, "\n\nAda wrote:\n> hello")
    }

    func testMissingSenderStillQuotes() {
        XCTAssertEqual(
            ReplyPrefill.quotedReplyBody(original: "hello", senderDisplay: nil, sentAt: nil),
            "\n\n> hello")
        let date = attributionDate(1_750_000_000)
        XCTAssertEqual(
            ReplyPrefill.quotedReplyBody(original: "hello", senderDisplay: nil,
                                         sentAt: 1_750_000_000),
            "\n\nOn \(date):\n> hello")
    }

    func testExistingQuotesNestToDoubleAngle() {
        let body = ReplyPrefill.quotedReplyBody(
            original: "top\n> quoted once\n>> quoted twice\n>tight quote",
            senderDisplay: "Ada", sentAt: nil)
        XCTAssertEqual(
            body,
            "\n\nAda wrote:\n> top\n>> quoted once\n>>> quoted twice\n>>tight quote")
    }

    func testLongBodiesAreNotTruncated() {
        let original = (1...1_000).map { "line \($0)" }.joined(separator: "\n")
        let body = ReplyPrefill.quotedReplyBody(original: original, senderDisplay: "Ada",
                                                sentAt: nil)
        XCTAssertTrue(body.contains("> line 1\n"))
        XCTAssertTrue(body.hasSuffix("> line 1000"))
        XCTAssertEqual(body.components(separatedBy: "\n> line ").count, 1_001)
    }

    func testTrailingNewlinesInOriginalDontLeaveEmptyQuoteLines() {
        let body = ReplyPrefill.quotedReplyBody(original: "hello\n\n", senderDisplay: "Ada",
                                                sentAt: nil)
        XCTAssertEqual(body, "\n\nAda wrote:\n> hello")
    }

    func testMakeWiresQuotedBodyWithSenderNameAndDate() {
        let detail = MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb",
                subject: "Plans", fromName: "Ada", fromAddress: "ada@x.test",
                sentAt: 1_750_000_000, snippet: nil, hasAttachments: false,
                hasBody: true,
                isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: "<orig-1@x.test>",
            addresses: [Address(kind: "from", name: "Ada", address: "ada@x.test")],
            textBody: "See you then.", sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
        let p = ReplyPrefill.make(detail: detail, mode: .reply, selfEmail: "me@x.test")
        let date = attributionDate(1_750_000_000)
        XCTAssertEqual(p.body, "\n\nOn \(date), Ada wrote:\n> See you then.")
    }

    func testMakeFallsBackToSenderAddressWhenNameMissing() {
        let detail = MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb",
                subject: "Plans", fromName: nil, fromAddress: "ada@x.test",
                sentAt: nil, snippet: nil, hasAttachments: false,
                hasBody: true,
                isRead: true, isStarred: false, threadCount: 1),
            rfc822MessageId: nil,
            addresses: [Address(kind: "from", name: nil, address: "ada@x.test")],
            textBody: "hello", sanitizedHtml: nil, hasRemoteContent: nil,
            attachments: nil, trust: nil)
        let p = ReplyPrefill.make(detail: detail, mode: .reply, selfEmail: "me@x.test")
        XCTAssertEqual(p.body, "\n\nada@x.test wrote:\n> hello")
    }
}
