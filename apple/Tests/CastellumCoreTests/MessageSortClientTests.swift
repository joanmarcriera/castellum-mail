// Castellum - client-side sort for the lists that are fully loaded in memory
// (, slice 4): the fold shared with the core, the ordering rules, and
// when the model applies them (search relevance until a header click, Snoozed due order,
// smart mailboxes keep the chosen sort).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class MessageSortClientTests: XCTestCase {
    private func row(_ id: String, name: String? = nil, address: String? = nil, subject: String? = nil,
                     sentAt: Int64? = nil) -> MessageHeader {
        MessageHeader(
            id: id, accountId: "a", mailboxId: "a:INBOX", subject: subject, fromName: name, fromAddress: address,
            sentAt: sentAt, snippet: nil, hasAttachments: false, hasBody: nil, isRead: true, isStarred: false,
            threadCount: 1)
    }

    /// The SAME table as `VECTORS` in `native/crates/castellum-db/src/sort_fold.rs`. Keep them equal:
    /// a fold change in either language must show up as a failure in the other suite.
    private let coreVectors: [(String, Bool, String)] = [
        ("Àlex", false, "alex"), ("Óscar", false, "oscar"), ("Ñu", false, "nu"), ("Zoe", false, "zoe"),
        ("L·L", false, "l·l"), ("ß", false, "ß"), ("đorđe", false, "dorde"), ("Łukasz", false, "lukasz"),
        ("Øystein", false, "oystein"), ("  padded  ", false, "padded"),
        ("RE: Fwd: RV: hola", true, "hola"), ("Re:Re:  hello", true, "hello"), ("AW: Ref: x", true, "x"),
        ("Fw: Fw: Fw: Fw: Fw: deep", true, "deep"), ("Re: hola", false, "re: hola"), ("Reunió", true, "reunio"),
        ("Fwdx: not a prefix", true, "fwdx: not a prefix"), ("", true, ""), ("   ", true, ""), ("re:", true, ""),
    ]

    func testFoldMatchesTheCoreVectors() {
        for (input, strip, want) in coreVectors {
            XCTAssertEqual(MessageSortClient.fold(input, stripPrefixes: strip), want, "input \(input.debugDescription) strip \(strip)")
        }
    }

    func testDecomposedInputFoldsLikeComposed() {
        XCTAssertEqual(MessageSortClient.fold("A\u{300}lex", stripPrefixes: false), "alex")
        XCTAssertEqual(MessageSortClient.fold("n\u{303}u", stripPrefixes: false), "nu")
    }

    func testFoldNeverCrashesOnOddInput() {
        for s in ["\u{300}\u{301}", "𝒜𝒷𝒸", "re:\u{0}re:", "\u{FEFF}Re: x"] {
            _ = MessageSortClient.fold(s, stripPrefixes: true)
            _ = MessageSortClient.fold(s, stripPrefixes: false)
        }
    }

    func testFromSortsByNameElseAddressFoldedAndTiesNewestFirst() {
        let rows = [
            row("1", name: "Zoe", sentAt: 10), row("2", name: "Àlex", sentAt: 20), row("3", name: "  ", address: "bob@x.test", sentAt: 30),
            row("4", name: "Ana", sentAt: 40), row("5", name: "Ana", sentAt: 50), row("6", address: nil, sentAt: 60),
        ]
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .from, ascending: true)).map(\.id),
                       ["6", "2", "5", "4", "3", "1"], "no sender folds to empty and is first; equal senders newest first")
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .from, ascending: false)).map(\.id),
                       ["1", "3", "5", "4", "2", "6"], "descending flips the sender only, not the date tie-break")
    }

    func testSubjectIgnoresReplyPrefixesAndAccents() {
        let rows = [row("1", subject: "Re: Zebra"), row("2", subject: "apple"), row("3", subject: "Fwd: Émile"),
                    row("4", subject: nil), row("5", subject: "Banana")]
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .subject, ascending: true)).map(\.id),
                       ["4", "2", "5", "3", "1"])
    }

    func testUndatedRowsAreLastInBothDateDirections() {
        let rows = [row("a", sentAt: nil), row("b", sentAt: 10), row("c", sentAt: 30), row("d", sentAt: nil), row("e", sentAt: 20)]
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .date, ascending: false)).map(\.id), ["c", "e", "b", "a", "d"])
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .date, ascending: true)).map(\.id), ["b", "e", "c", "a", "d"])
    }

    func testFlagKeysGroupThenNewestFirst() {
        func flagged(_ id: String, read: Bool, star: Bool, att: Bool, at: Int64) -> MessageHeader {
            MessageHeader(
                id: id, accountId: "a", mailboxId: "a:INBOX", subject: id, fromName: nil, fromAddress: nil,
                sentAt: at, snippet: nil, hasAttachments: att, hasBody: nil, isRead: read, isStarred: star, threadCount: 1)
        }
        let rows = [
            flagged("a", read: true, star: false, att: false, at: 10), flagged("b", read: false, star: true, att: true, at: 20),
            flagged("c", read: true, star: true, att: false, at: 30), flagged("d", read: false, star: false, att: true, at: 40),
        ]
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .unread, ascending: true)).map(\.id), ["d", "b", "c", "a"])
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .unread, ascending: false)).map(\.id), ["c", "a", "d", "b"])
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .starred, ascending: false)).map(\.id), ["c", "b", "d", "a"])
        XCTAssertEqual(MessageSortClient.sorted(rows, by: MessageSort(key: .attachments, ascending: false)).map(\.id), ["d", "b", "c", "a"])
    }

    func testSortIsDeterministicForEqualKeys() {
        let rows = (0..<6).map { row("id\($0)", name: "Same", sentAt: 5) }
        let a = MessageSortClient.sorted(rows, by: MessageSort(key: .from, ascending: true)).map(\.id)
        XCTAssertEqual(a, MessageSortClient.sorted(rows.reversed(), by: MessageSort(key: .from, ascending: true)).map(\.id))
        XCTAssertEqual(a, a.sorted(), "ties end on id")
    }
}

@MainActor
final class MessageSortNonPagedListTests: XCTestCase {
    private var models: [AppModel] = []
    private var suites: [String] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-clientsort-\(UUID().uuidString)", isDirectory: true)
        let model = AppModel(profileDirectory: dir)
        let name = "castellum-clientsort-\(UUID().uuidString)"
        suites.append(name)
        model.messageSortDefaults = UserDefaults(suiteName: name)!
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for name in suites { UserDefaults.standard.removePersistentDomain(forName: name) }
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func row(_ id: String, name: String, sentAt: Int64) -> MessageHeader {
        MessageHeader(
            id: id, accountId: "a", mailboxId: "a:INBOX", subject: id, fromName: name, fromAddress: nil,
            sentAt: sentAt, snippet: nil, hasAttachments: false, hasBody: nil, isRead: true, isStarred: false,
            threadCount: 1)
    }

    private let rows: [(String, String, Int64)] = [("m1", "Zoe", 10), ("m2", "Ana", 20), ("m3", "Bob", 30)]
    private func headers(_ model: AppModel) -> [MessageHeader] { rows.map { row($0.0, name: $0.1, sentAt: $0.2) } }

    func testSearchKeepsRelevanceUntilAHeaderIsClicked() {
        let model = makeModel()
        model.searchText = "x"
        XCTAssertNil(model.displaySort, "relevance order, no arrow")
        XCTAssertEqual(model.clientSorted(headers(model)).map(\.id), ["m1", "m2", "m3"], "left in the order the core returned")
        model.setMessageSort(MessageSort(key: .from, ascending: true))
        XCTAssertTrue(model.searchSortOverridden)
        XCTAssertEqual(model.displaySort, MessageSort(key: .from, ascending: true))
        XCTAssertEqual(model.clientSorted(headers(model)).map(\.id), ["m2", "m3", "m1"])
    }

    func testAHeaderClickNamingTheStoredSortStillOverridesRelevance() {
        let model = makeModel()
        model.setMessageSort(MessageSort(key: .from, ascending: true))   // stored before the search
        model.searchText = "x"
        XCTAssertNil(model.displaySort)
        model.setMessageSort(MessageSort(key: .from, ascending: true))   // the same sort, clicked in the search
        XCTAssertTrue(model.searchSortOverridden)
    }

    func testNewQueryRestoresRelevance() {
        let model = makeModel()
        model.searchText = "x"
        model.setMessageSort(MessageSort(key: .subject, ascending: true))
        XCTAssertNotNil(model.displaySort)
        model.searchText = "xy"
        XCTAssertFalse(model.searchSortOverridden)
        XCTAssertNil(model.displaySort)
        XCTAssertEqual(model.messageSort, MessageSort(key: .subject, ascending: true), "the stored sort itself is untouched")
    }

    func testSnoozedKeepsSoonestDueOrderUntilASortIsChosen() {
        let model = makeModel()
        model.select(snoozed: ())
        XCTAssertNil(model.displaySort, "the due order is Snoozed's natural order")
        XCTAssertEqual(model.clientSorted(headers(model)).map(\.id), ["m1", "m2", "m3"])
        model.setMessageSort(MessageSort(key: .date, ascending: true))
        XCTAssertEqual(model.displaySort, MessageSort(key: .date, ascending: true))
        XCTAssertEqual(model.clientSorted(headers(model).reversed()).map(\.id), ["m1", "m2", "m3"])
    }

    func testSortableColumnsInSentExcludeFromForClientListsToo() {
        let model = makeModel()
        model.selectedMailbox = Mailbox(id: "a:Sent", accountId: "a", path: "Sent", name: "Sent", role: "sent",
                                        delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
        XCTAssertEqual(model.sortableColumnIds, ["date", "subject", "unread", "star", "attach"])
    }

    /// A smart mailbox reloads through the core and keeps the chosen sort (client-side).
    func testSmartMailboxReloadKeepsTheSort() async throws {
        let model = makeModel()
        let accountId = "smartsort-\(UUID().uuidString)"
        let mailboxId = "\(accountId):INBOX"
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId, email: "ss@local.test",
            mailboxId: mailboxId, role: "inbox")
        for (i, from) in ["zoe@x.test", "ana@x.test", "bob@x.test"].enumerated() {
            try DirectFixture.seedMessage(
                profileDirectory: model.profileDirectory, id: "\(accountId):m\(i)", accountId: accountId,
                mailboxId: mailboxId, subject: "News \(i)", fromAddress: from,
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false,
                sentAt: Int64(1_700_000_000 + i))
        }
        let group = FilterGroupDTO(op: .and, conditions: [
            FilterConditionDTO(field: .subject, op: .contains, value: .text("News")),
        ])
        let req = AppModel.SmartMailboxUpsertRequest(
            id: nil, name: "News", query: "", symbol: nil, group: group, scope: nil)
        let sm = try model.core.upsertSmartMailbox(json: try XCTUnwrap(AppModel.encodeSmartMailboxUpsert(req)))
        model.smartMailboxes = [sm]
        model.setMessageSort(MessageSort(key: .from, ascending: true))
        model.select(smartMailbox: sm)
        let want = ["\(accountId):m1", "\(accountId):m2", "\(accountId):m0"]   // ana, bob, zoe
        let end = Date().addingTimeInterval(8)
        while model.messages.map(\.id) != want && Date() < end { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertEqual(model.messages.map(\.id), want)
        model.reloadCurrentMessages()   // what a sync does
        try await Task.sleep(nanoseconds: 300_000_000)
        XCTAssertEqual(model.messages.map(\.id), want, "the reload keeps the From sort")
        model.setMessageSort(MessageSort(key: .from, ascending: false))
        let desc = want.reversed() as [String]
        let end2 = Date().addingTimeInterval(8)
        while model.messages.map(\.id) != desc && Date() < end2 { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertEqual(model.messages.map(\.id), desc)
    }
}
