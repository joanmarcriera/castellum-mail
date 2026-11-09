// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// MM-2: scope planning and header drill-down query text.
final class SearchScopeTests: XCTestCase {
    private func mailbox(_ path: String, account: String = "a1") -> Mailbox {
        Mailbox(id: "\(account):\(path)", accountId: account, path: path, name: path,
                role: nil, delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    func testQuotingKeepsValuesTokenSafe() {
        XCTAssertEqual(SearchQueryBuilder.quote("alice@x.test"), "alice@x.test")
        XCTAssertEqual(SearchQueryBuilder.quote("Q3 budget: final"), "\"Q3 budget: final\"")
        XCTAssertEqual(SearchQueryBuilder.quote("say \"hi\" now"), "\"say \\\"hi\\\" now\"", "a quote is escaped, not dropped")
        XCTAssertEqual(SearchQueryBuilder.quote("\"Budget\""), "\"\\\"Budget\\\"\"")
        XCTAssertEqual(SearchQueryBuilder.quote("a b\\c"), "\"a b\\\\c\"")
        XCTAssertEqual(SearchQueryBuilder.tokens(of: "subject:\"Re: \\\"Budget\\\" v2\" from:=a@x.test").count, 2, "an escaped quote does not end the span")
        XCTAssertEqual(SearchQueryBuilder.quote("-urgent"), "\"-urgent\"", "a leading dash would negate")
        XCTAssertEqual(SearchQueryBuilder.quote("  padded  "), "padded")
    }

    func testDrillDownAppendsAndDeduplicates() {
        XCTAssertEqual(SearchQueryBuilder.narrowing("", qualifier: "from", value: "a@x.test"), "from:a@x.test")
        XCTAssertEqual(SearchQueryBuilder.narrowing("invoice", qualifier: "from", value: "a@x.test"), "invoice from:a@x.test")
        XCTAssertEqual(
            SearchQueryBuilder.narrowing("from:a@x.test", qualifier: "from", value: "a@x.test"), "from:a@x.test",
            "narrowing twice by the same thing changes nothing")
        XCTAssertEqual(
            SearchQueryBuilder.narrowing("from:a@x.test", qualifier: "to", value: "b@x.test"),
            "from:a@x.test to:b@x.test", "different qualifiers combine")
    }

    func testSingleValuedQualifiersReplace() {
        XCTAssertEqual(
            SearchQueryBuilder.narrowing("date:2026-03-01 from:a", qualifier: "date", value: "2026-03-15"),
            "from:a date:2026-03-15")
    }

    func testQuotedSpansStayOneToken() {
        XCTAssertEqual(
            SearchQueryBuilder.tokens(of: "subject:\"a b\" -body:\"x y\" plain"),
            ["subject:\"a b\"", "-body:\"x y\"", "plain"])
        XCTAssertEqual(
            SearchQueryBuilder.narrowing("subject:\"a b\"", qualifier: "subject", value: "a b"),
            "subject:\"a b\"")
    }

    func testDayTokenIsTheUTCDayTheCoreWillSearch() {
        // 2026-03-16 01:00 UTC is still 15 March in the Americas; the token
        // must be the UTC day so the clicked message is inside `date:`.
        XCTAssertEqual(SearchQueryBuilder.dayToken(epoch: 1_773_622_800), "2026-03-16")
        XCTAssertEqual(SearchQueryBuilder.dayToken(epoch: 1_773_576_000), "2026-03-15")
    }

    func testPlanPerScope() {
        let inbox = mailbox("INBOX")
        XCTAssertEqual(
            SearchQueryBuilder.plan(scope: .account, text: "x", selectedMailbox: inbox, firstAccountId: "a9", crossAccountSelection: false),
            SearchPlan(accountId: "a1", query: "x"))
        XCTAssertEqual(
            SearchQueryBuilder.plan(scope: .mailbox, text: "x", selectedMailbox: inbox, firstAccountId: "a9", crossAccountSelection: false),
            SearchPlan(accountId: "a1", query: "x path:INBOX", mailboxId: "a1:INBOX"))
        XCTAssertEqual(
            SearchQueryBuilder.plan(scope: .mailbox, text: "x", selectedMailbox: mailbox("Clients/Big Co"), firstAccountId: nil, crossAccountSelection: false),
            SearchPlan(accountId: "a1", query: "x path:\"Clients/Big Co\"", mailboxId: "a1:Clients/Big Co"))
        XCTAssertEqual(
            SearchQueryBuilder.plan(scope: .all, text: "x", selectedMailbox: inbox, firstAccountId: "a9", crossAccountSelection: false),
            SearchPlan(accountId: nil, query: "x"))
    }

    func testUnifiedAndSmartSelectionsAlwaysSearchEverywhere() {
        for scope in SearchScope.allCases {
            XCTAssertEqual(
                SearchQueryBuilder.plan(scope: scope, text: "x", selectedMailbox: nil, firstAccountId: "a1", crossAccountSelection: true),
                SearchPlan(accountId: nil, query: "x"))
        }
    }

    func testNoMailboxFallsBackToFirstAccountAndNothingSelectedPlansNothing() {
        XCTAssertEqual(
            SearchQueryBuilder.plan(scope: .mailbox, text: "x", selectedMailbox: nil, firstAccountId: "a9", crossAccountSelection: false),
            SearchPlan(accountId: "a9", query: "x"))
        XCTAssertNil(SearchQueryBuilder.plan(scope: .account, text: "x", selectedMailbox: nil, firstAccountId: nil, crossAccountSelection: false))
    }

    /// The text the app builds must round-trip through the real core grammar:
    /// save it as a smart mailbox and run it over the actual FFI.
    func testGeneratedQueriesAreAcceptedByTheCore() throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-swift-scope-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        let q = ["from:a@x.test", "to:b@x.test", "cc:c@x.test", "date:2026-03-15", "path:\"Clients/Big Co\"",
                 "subject:\"Q3 budget: final\"", "body:hello within:7d size:>1mb flag:unanswered"]
            .joined(separator: " ")
        XCTAssertNoThrow(try core.searchAcrossAccounts(query: q, limit: 10))
        XCTAssertNoThrow(try core.searchAcrossAccounts(query: "attachment:x.pdf bcc:z date:2026-01..2026-03", limit: 10))
    }
}
