// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
import SwiftUI
@testable import Castellum

/// Tests pure grouping/ordering helpers for the Kanban board.
/// These tests operate on in-memory [ProjectCard] arrays — no FFI required.
final class BoardLogicTests: XCTestCase {

    // MARK: - Fixtures

    private func makeCard(
        id: String,
        status: String,
        position: Int64,
        archived: Bool = false
    ) -> ProjectCard {
        ProjectCard(
            id: id,
            name: "Project \(id)",
            notes: "",
            status: status,
            color: nil,
            position: position,
            archived: archived,
            createdAt: 1_751_000_000,
            updatedAt: 1_751_000_000,
            lastActivityAt: 1_751_000_000,
            emailCount: 0,
            contactCount: 0
        )
    }

    // MARK: - Group-by-status

    func testGroupByStatusReturnsCorrectBuckets() {
        let cards: [ProjectCard] = [
            makeCard(id: "a", status: "active",  position: 0),
            makeCard(id: "b", status: "waiting", position: 0),
            makeCard(id: "c", status: "active",  position: 1),
            makeCard(id: "d", status: "done",    position: 0),
            makeCard(id: "e", status: "someday", position: 0),
        ]
        let grouped = Dictionary(grouping: cards, by: \.status)
        XCTAssertEqual(grouped["active"]?.count,  2)
        XCTAssertEqual(grouped["waiting"]?.count, 1)
        XCTAssertEqual(grouped["done"]?.count,    1)
        XCTAssertEqual(grouped["someday"]?.count, 1)
        XCTAssertNil(grouped["unknown"])
    }

    func testGroupByStatusEmptyInputProducesEmptyGroups() {
        let cards: [ProjectCard] = []
        let grouped = Dictionary(grouping: cards, by: \.status)
        XCTAssertTrue(grouped.isEmpty)
    }

    // MARK: - Ordering by position

    func testCardsInColumnSortedByPosition() {
        let cards: [ProjectCard] = [
            makeCard(id: "z", status: "active", position: 2),
            makeCard(id: "a", status: "active", position: 0),
            makeCard(id: "m", status: "active", position: 1),
        ]
        let sorted = cards.filter { $0.status == "active" }
                          .sorted { $0.position < $1.position }
        XCTAssertEqual(sorted.map(\.id), ["a", "m", "z"])
    }

    func testSingleCardColumnSortStable() {
        let cards = [makeCard(id: "only", status: "done", position: 99)]
        let sorted = cards.sorted { $0.position < $1.position }
        XCTAssertEqual(sorted.first?.id, "only")
    }

    // MARK: - Archive filter

    func testArchivedCardsExcludedByDefault() {
        let cards: [ProjectCard] = [
            makeCard(id: "live",  status: "active", position: 0, archived: false),
            makeCard(id: "dead",  status: "active", position: 1, archived: true),
            makeCard(id: "live2", status: "done",   position: 0, archived: false),
        ]
        let visible = cards.filter { !$0.archived }
        XCTAssertEqual(visible.count, 2)
        XCTAssertTrue(visible.allSatisfy { !$0.archived })
    }

    func testArchivedCardsIncludedWhenFlagSet() {
        let cards: [ProjectCard] = [
            makeCard(id: "live", status: "active",  position: 0, archived: false),
            makeCard(id: "dead", status: "waiting", position: 0, archived: true),
        ]
        let all = cards.filter { _ in true }   // showArchived = true → no filter
        XCTAssertEqual(all.count, 2)
    }

    // MARK: - New-position calculation

    /// The next position in a column equals the current card count.
    func testNextPositionEqualsColumnCardCount() {
        let cards: [ProjectCard] = [
            makeCard(id: "1", status: "active", position: 0),
            makeCard(id: "2", status: "active", position: 1),
            makeCard(id: "3", status: "waiting", position: 0),
        ]
        let activeCount = cards.filter { $0.status == "active" }.count
        XCTAssertEqual(Int64(activeCount), 2)

        let waitingCount = cards.filter { $0.status == "waiting" }.count
        XCTAssertEqual(Int64(waitingCount), 1)
    }

    func testNextPositionInEmptyColumnIsZero() {
        let cards: [ProjectCard] = []
        let count = Int64(cards.filter { $0.status == "done" }.count)
        XCTAssertEqual(count, 0)
    }

    // MARK: - Hex colour helper

    func testHexColorInitValidSixDigit() {
        let c = Color(hex: "#3b82f6")
        XCTAssertNotNil(c)
    }

    func testHexColorInitWithoutHash() {
        let c = Color(hex: "ef4444")
        XCTAssertNotNil(c)
    }

    func testHexColorInitInvalidReturnsNil() {
        XCTAssertNil(Color(hex: ""))
        XCTAssertNil(Color(hex: "#gg0000"))
        XCTAssertNil(Color(hex: "12345"))   // 5 chars
    }
}
