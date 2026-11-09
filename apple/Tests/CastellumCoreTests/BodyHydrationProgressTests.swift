// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// #3631 G5: per-mailbox body-hydration progress (DTO decode, FFI wrapper on
/// a real temp profile, sidebar tooltip presentation).
final class BodyHydrationProgressTests: XCTestCase {
    func testTooltipOnlyWhenBodiesAreMissing() {
        func p(_ hydrated: Int64, _ total: Int64) -> BodyHydrationProgress {
            BodyHydrationProgress(mailboxId: "a:INBOX", total: total, hydrated: hydrated)
        }
        XCTAssertNil(BodyHydrationPresentation.tooltip(nil), "unknown mailbox: no tooltip")
        XCTAssertNil(BodyHydrationPresentation.tooltip(p(0, 0)), "empty mailbox: nothing to download")
        XCTAssertNil(BodyHydrationPresentation.tooltip(p(7, 7)), "fully hydrated: stay quiet")
        let partial = BodyHydrationPresentation.tooltip(p(25, 8000))
        XCTAssertNotNil(partial)
        XCTAssertTrue(partial!.contains("25") && partial!.contains("8000") || partial!.contains("8,000"),
                      "tooltip shows hydrated and total: \(partial!)")
    }

    func testDecodesCoreJSON() throws {
        let json = #"[{"mailbox_id":"a:INBOX","total":10,"hydrated":4}]"#
        let dec = JSONDecoder()
        dec.keyDecodingStrategy = .convertFromSnakeCase
        let rows = try dec.decode([BodyHydrationProgress].self, from: Data(json.utf8))
        XCTAssertEqual(rows, [BodyHydrationProgress(mailboxId: "a:INBOX", total: 10, hydrated: 4)])
    }

    func testCoreWrapperReturnsEmptyForUnknownAccount() throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-swift-test-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        XCTAssertEqual(try core.bodyHydrationProgress(accountId: "nobody"), [])
    }

    // MARK: - #3633 G13: search body-coverage note

    func testSearchNoteOnlyWhenBodiesAreMissing() {
        XCTAssertNil(BodyHydrationPresentation.searchNote(nil))
        XCTAssertNil(BodyHydrationPresentation.searchNote(BodyHydrationScope(total: 0, missing: 0)),
                     "empty scope: no note")
        XCTAssertNil(BodyHydrationPresentation.searchNote(BodyHydrationScope(total: 12, missing: 0)),
                     "fully hydrated scope: no note")
        let note = BodyHydrationPresentation.searchNote(BodyHydrationScope(total: 100, missing: 37))
        XCTAssertNotNil(note)
        XCTAssertTrue(note!.contains("37"), "note carries the count from the core: \(note!)")
    }

    func testScopeCoreWrapperCoversAllShapes() throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-swift-test-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        let empty = BodyHydrationScope(total: 0, missing: 0)
        XCTAssertEqual(try core.bodyHydrationScope(accountId: nil, mailboxId: nil), empty)
        XCTAssertEqual(try core.bodyHydrationScope(accountId: "nobody", mailboxId: nil), empty)
        XCTAssertEqual(try core.bodyHydrationScope(accountId: "nobody", mailboxId: "nobody:INBOX"), empty)
        XCTAssertEqual(try core.bodyHydrationScope(accountId: nil, mailboxId: nil, query: "x -in:trash"), empty)
    }

    func testMailboxScopePlanCarriesMailboxId() {
        let inbox = Mailbox(id: "a1:INBOX", accountId: "a1", path: "INBOX", name: "INBOX",
                            role: nil, delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
        let mailboxPlan = SearchQueryBuilder.plan(
            scope: .mailbox, text: "x", selectedMailbox: inbox, firstAccountId: nil, crossAccountSelection: false)
        XCTAssertEqual(mailboxPlan?.mailboxId, "a1:INBOX")
        let accountPlan = SearchQueryBuilder.plan(
            scope: .account, text: "x", selectedMailbox: inbox, firstAccountId: nil, crossAccountSelection: false)
        XCTAssertNil(accountPlan?.mailboxId)
    }
}
