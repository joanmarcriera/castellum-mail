// Castellum - the removal-undo banner (, slice 2): its pure state
// machine, the CA/ES/EN copy with singular and plural forms, and the AppModel
// wiring (offered after Archive, "Returning…" after Undo, never over the
// undo-send banner).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class RemovalUndoBannerStateTests: XCTestCase {
    private let id = UUID()

    private func offered(count: Int = 3) -> RemovalUndoBannerState {
        var s = RemovalUndoBannerState()
        s.offer(entryId: id, verb: .archive, count: count, accountIds: ["a1"])
        return s
    }

    func testOfferShowsForEightSecondsThenHides() {
        var s = offered()
        XCTAssertEqual(s.shown?.phase, .offering(remaining: 8))
        for _ in 0..<7 { s.tick() }
        XCTAssertEqual(s.shown?.phase, .offering(remaining: 1))
        s.tick()
        XCTAssertFalse(s.isVisible)
    }

    func testZeroCountOffersNothing() {
        XCTAssertFalse(offered(count: 0).isVisible)
    }

    func testASecondActionReplacesTheFirst() {
        var s = offered()
        let second = UUID()
        s.offer(entryId: second, verb: .delete, count: 1, accountIds: ["a2"])
        XCTAssertEqual(s.shown?.entryId, second)
        XCTAssertEqual(s.shown?.verb, .delete)
        // The first entry's Undo (Cmd-Z) must not hijack the new banner.
        s.undoRequested(entryId: id, destinationName: "Inbox")
        XCTAssertEqual(s.shown?.phase, .offering(remaining: 8))
    }

    func testDismissHides() {
        var s = offered()
        s.dismiss()
        XCTAssertFalse(s.isVisible)
    }

    func testReturningClearsOnTheFirstSyncAfterTheReplay() {
        var s = offered()
        s.undoRequested(entryId: id, destinationName: "Inbox")
        XCTAssertEqual(s.shown?.phase, .returning(remaining: 30, replayed: false))
        XCTAssertEqual(s.shown?.destinationName, "Inbox")
        s.syncCompleted(accountId: "a1") // an unrelated sync before the replay landed
        XCTAssertTrue(s.isVisible)
        s.replayApplied(accountIds: ["other"])
        s.syncCompleted(accountId: "a1")
        XCTAssertTrue(s.isVisible, "a replay on another account says nothing about this one")
        s.replayApplied(accountIds: ["a1"])
        s.syncCompleted(accountId: "other")
        XCTAssertTrue(s.isVisible)
        s.syncCompleted(accountId: "a1")
        XCTAssertFalse(s.isVisible)
    }

    func testReturningClearsAfterThirtySeconds() {
        var s = offered()
        s.undoRequested(entryId: id, destinationName: nil)
        for _ in 0..<29 { s.tick() }
        XCTAssertTrue(s.isVisible)
        s.tick()
        XCTAssertFalse(s.isVisible)
    }

    func testClearOnlyAffectsItsOwnEntry() {
        var s = offered()
        s.clear(entryId: UUID())
        XCTAssertTrue(s.isVisible)
        s.clear(entryId: id)
        XCTAssertFalse(s.isVisible)
    }

    // MARK: - copy

    private func lproj(_ lang: String) throws -> Bundle {
        let path = try XCTUnwrap(Bundle(for: AppModel.self).path(forResource: lang, ofType: "lproj"), lang)
        return try XCTUnwrap(Bundle(path: path))
    }

    func testOfferCopyHasSingularAndPluralInEveryLanguage() throws {
        let expected: [String: [(RemovalVerb, Int, String)]] = [
            "en": [(.archive, 1, "Archived 1 message"), (.archive, 3, "Archived 3 messages"),
                   (.delete, 1, "Moved 1 message to Trash"), (.delete, 2, "Moved 2 messages to Trash"),
                   (.move, 1, "Moved 1 message"), (.move, 5, "Moved 5 messages")],
            "ca": [(.archive, 1, "S’ha arxivat 1 missatge"), (.archive, 3, "S’han arxivat 3 missatges"),
                   (.delete, 1, "S’ha mogut 1 missatge a la paperera"),
                   (.delete, 2, "S’han mogut 2 missatges a la paperera"),
                   (.move, 1, "S’ha mogut 1 missatge"), (.move, 5, "S’han mogut 5 missatges")],
            "es": [(.archive, 1, "Se ha archivado 1 mensaje"), (.archive, 3, "Se han archivado 3 mensajes"),
                   (.delete, 1, "Se ha movido 1 mensaje a la papelera"),
                   (.delete, 2, "Se han movido 2 mensajes a la papelera"),
                   (.move, 1, "Se ha movido 1 mensaje"), (.move, 5, "Se han movido 5 mensajes")],
        ]
        for (lang, cases) in expected {
            let bundle = try lproj(lang)
            for (verb, count, text) in cases {
                XCTAssertEqual(
                    RemovalUndoBannerState.offerText(verb: verb, count: count, bundle: bundle), text,
                    "\(lang) \(verb) \(count)")
            }
        }
    }

    func testReturningCopyIsTranslated() throws {
        XCTAssertEqual(
            RemovalUndoBannerState.returningText(destination: "Inbox", bundle: try lproj("ca")),
            "S’està retornant a Inbox…")
        XCTAssertEqual(
            RemovalUndoBannerState.returningText(destination: "Inbox", bundle: try lproj("es")),
            "Volviendo a Inbox…")
        XCTAssertEqual(
            RemovalUndoBannerState.returningText(destination: nil, bundle: try lproj("es")),
            "Volviendo a tu buzón…")
        XCTAssertEqual(
            RemovalUndoBannerState.returningText(destination: "Inbox", bundle: try lproj("en")),
            "Returning to Inbox…")
    }
}
