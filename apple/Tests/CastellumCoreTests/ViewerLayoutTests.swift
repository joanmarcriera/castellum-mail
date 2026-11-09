// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// MM-4: layout preference, correspondence merge and stats.
final class ViewerLayoutTests: XCTestCase {
    private func header(_ id: String, from: String?, at: Int64?) -> MessageHeader {
        MessageHeader(
            id: id, accountId: "a1", mailboxId: "a1:INBOX", subject: id, fromName: nil,
            fromAddress: from, sentAt: at, snippet: nil, hasAttachments: false, hasBody: nil,
            isRead: true, isStarred: false, threadCount: 1)
    }

    func testLayoutPersistsAndDefaultsToWidescreen() {
        let suite = "ViewerLayoutTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        MainActor.assumeIsolated {
            XCTAssertEqual(Preferences(defaults: defaults).viewerLayout, .widescreen)
            Preferences(defaults: defaults).viewerLayout = .classic
            XCTAssertEqual(Preferences(defaults: defaults).viewerLayout, .classic)
        }
        defaults.set("nonsense", forKey: "viewerLayout")
        MainActor.assumeIsolated {
            XCTAssertEqual(Preferences(defaults: defaults).viewerLayout, .widescreen)
        }
    }

    func testMergeDedupesAndSortsNewestFirstWithUndatedLast() {
        let a = header("a", from: "x@y.test", at: 100)
        let b = header("b", from: "x@y.test", at: 300)
        let c = header("c", from: nil, at: nil)
        let merged = Correspondence.merge([[a, b], [b, c], [a]])
        XCTAssertEqual(merged.map(\.id), ["b", "a", "c"])
    }

    func testQueriesQuoteTheAddress() {
        // Exact address (leading "="), not a contains-match.
        XCTAssertEqual(Correspondence.queries(for: "ana@x.test"), ["from:=ana@x.test", "to:=ana@x.test", "cc:=ana@x.test"])
        XCTAssertEqual(Correspondence.queries(for: "Ana Bo").first, "from:\"=Ana Bo\"")
    }

    func testStatsCountSentReceivedAndSpan() {
        let day: Int64 = 86_400
        let base: Int64 = 1_773_576_000 // 2026-03-15 UTC
        let msgs = [
            header("1", from: "them@x.test", at: base),
            header("2", from: "ME@example.com", at: base + day),
            header("3", from: "them@x.test", at: base + 40 * day),
            header("4", from: "them@x.test", at: nil),
        ]
        let s = Correspondence.stats(msgs, myAddresses: ["me@example.com"])
        XCTAssertEqual(s.total, 4)
        XCTAssertEqual(s.sent, 1, "case-insensitive match on my address")
        XCTAssertEqual(s.received, 3)
        XCTAssertEqual(s.first, Date(timeIntervalSince1970: TimeInterval(base)))
        XCTAssertEqual(s.last, Date(timeIntervalSince1970: TimeInterval(base + 40 * day)))
    }

    func testMonthlySeriesIsTwelveMonthsEndingAtTheNewestMessage() {
        let march: Int64 = 1_773_576_000       // 2026-03-15
        let april: Int64 = march + 40 * 86_400 // 2026-04-24
        let s = Correspondence.stats(
            [header("1", from: "a", at: march), header("2", from: "a", at: march + 100), header("3", from: "a", at: april)],
            myAddresses: [])
        XCTAssertEqual(s.perMonth.count, Correspondence.monthsShown)
        XCTAssertEqual(s.perMonth.last?.month, "2026-04")
        XCTAssertEqual(s.perMonth.last?.count, 1)
        XCTAssertEqual(s.perMonth[s.perMonth.count - 2].month, "2026-03")
        XCTAssertEqual(s.perMonth[s.perMonth.count - 2].count, 2)
        XCTAssertEqual(s.perMonth.first?.month, "2025-05")
        XCTAssertEqual(s.busiestMonth?.month, "2026-03")
    }

    func testEmptyInputHasNoSeries() {
        let s = Correspondence.stats([], myAddresses: ["me@x"])
        XCTAssertEqual(s, CorrespondenceStats())
        XCTAssertTrue(s.perMonth.isEmpty)
    }

    func testLayoutCommandsAreRegisteredInTheViewMenu() {
        let ids = CommandRegistry.all.filter { $0.menu?.name == .view }.map(\.id)
        XCTAssertTrue(ids.contains(.layoutWidescreen))
        XCTAssertTrue(ids.contains(.layoutClassic))
        XCTAssertTrue(ids.contains(.showCorrespondence))
    }

    private func detail(from: String?, to: [String], cc: [String] = []) -> MessageDetail {
        var addresses = to.map { Address(kind: "to", name: nil, address: $0) }
        addresses += cc.map { Address(kind: "cc", name: nil, address: $0) }
        return MessageDetail(
            header: header("d", from: from, at: 1), rfc822MessageId: nil, addresses: addresses,
            textBody: nil, sanitizedHtml: nil)
    }

    func testOtherPartyIsTheSenderUnlessTheSenderIsMe() {
        let mine: Set<String> = ["me@example.com"]
        XCTAssertEqual(Correspondence.otherParty(of: detail(from: "ana@x.test", to: ["me@example.com"]), myAddresses: mine)?.address, "ana@x.test")
        // A message I sent: the first recipient who is not me.
        XCTAssertEqual(Correspondence.otherParty(of: detail(from: "ME@example.com", to: ["me@example.com", "bo@x.test"]), myAddresses: mine)?.address, "bo@x.test")
        XCTAssertEqual(Correspondence.otherParty(of: detail(from: "me@example.com", to: [], cc: ["cy@x.test"]), myAddresses: mine)?.address, "cy@x.test")
        XCTAssertNil(Correspondence.otherParty(of: detail(from: "me@example.com", to: ["me@example.com"]), myAddresses: mine))
    }

    func testCappedStatsDifferFromUncapped() {
        var a = CorrespondenceStats()
        var b = CorrespondenceStats()
        XCTAssertEqual(a, b)
        b.isCapped = true
        XCTAssertNotEqual(a, b)
        a.isCapped = true
        XCTAssertEqual(a, b)
    }

    func testMergeCappedCutsAtTheNewestOldestOfTheCappedLists() {
        // "from" hit its cap of 3 (oldest row = 200); "to" is uncapped and has
        // older mail (100, 50) that the capped list cannot vouch for.
        let from = [header("f1", from: "t@x", at: 500), header("f2", from: "t@x", at: 300), header("f3", from: "t@x", at: 200)]
        let to = [header("t1", from: "me", at: 400), header("t2", from: "me", at: 100), header("t3", from: "me", at: 50)]
        let (messages, capped) = Correspondence.mergeCapped([from, to], cap: 3)
        XCTAssertTrue(capped)
        // "to" also has exactly 3 rows, so it is capped too (oldest 50); the
        // cutoff is the newest of the oldests: 200.
        XCTAssertEqual(messages.map(\.id), ["f1", "t1", "f2", "f3"])
    }

    func testMergeCappedLeavesUncappedListsUntouched() {
        let a = [header("a", from: "x", at: 10)]
        let b = [header("b", from: "x", at: 5)]
        let (messages, capped) = Correspondence.mergeCapped([a, b], cap: 500)
        XCTAssertFalse(capped)
        XCTAssertEqual(messages.map(\.id), ["a", "b"])
    }

    func testMergeCappedOnlyOneListCapped() {
        let from = [header("f1", from: "t@x", at: 500), header("f2", from: "t@x", at: 300)]
        let to = [header("t1", from: "me", at: 400), header("t2", from: "me", at: 100)]
        let (messages, capped) = Correspondence.mergeCapped([from, to, []], cap: 2)
        XCTAssertTrue(capped)
        XCTAssertEqual(messages.map(\.id), ["f1", "t1", "f2"]) // t2 (100) is older than 300
    }

    func testMyAddressesCombinesAccountsAndIdentitiesLowercased() {
        let mine = Correspondence.myAddresses(accountEmails: ["Me@Example.com"], identityEmails: ["Alias@Example.com", "me@example.com"])
        XCTAssertEqual(mine, ["me@example.com", "alias@example.com"])
    }
}
