// Castellum - the pane-aware column layout that keeps Received on screen (#3042).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure arithmetic over `MessageListColumnLayout`, plus one test that pins the
// per-column overhead against a REAL `.inset` NSTableView so an AppKit change
// to cell spacing fails loudly instead of silently pushing Received off-screen.

import AppKit
import XCTest
@testable import Castellum

final class MessageListColumnLayoutTests: XCTestCase {
    typealias Layout = MessageListColumnLayout

    /// The factory columns exactly as `makeNSView` creates them.
    static func factory() -> [Layout.Column] {
        kColumns.map { Layout.Column(id: $0.id, width: $0.width, min: $0.min, max: $0.max, hidden: false) }
    }

    private func width(_ cols: [Layout.Column], _ id: String) -> CGFloat {
        cols.first { $0.id == id }!.width
    }

    func testShrinkOrderIsPreviewThenSubjectThenFrom() {
        let cols = Self.factory()
        let total = Layout.totalWidth(cols)
        // A small overflow comes out of Preview alone.
        let a = Layout.shrinkToFit(cols, available: total - 40)
        XCTAssertEqual(width(a, "preview"), width(cols, "preview") - 40, accuracy: 0.01)
        XCTAssertEqual(width(a, "subject"), width(cols, "subject"))
        XCTAssertEqual(width(a, "from"), width(cols, "from"))
        // Past Preview's slack, Subject gives next; From is still untouched.
        let slackPreview = width(cols, "preview") - cols.first { $0.id == "preview" }!.min
        let b = Layout.shrinkToFit(cols, available: total - slackPreview - 30)
        XCTAssertEqual(width(b, "preview"), cols.first { $0.id == "preview" }!.min, accuracy: 0.01)
        XCTAssertEqual(width(b, "subject"), width(cols, "subject") - 30, accuracy: 0.01)
        XCTAssertEqual(width(b, "from"), width(cols, "from"))
        // Then From.
        let slackSubject = width(cols, "subject") - cols.first { $0.id == "subject" }!.min
        let c = Layout.shrinkToFit(cols, available: total - slackPreview - slackSubject - 20)
        XCTAssertEqual(width(c, "from"), width(cols, "from") - 20, accuracy: 0.01)
    }

    func testShrinkNeverGoesBelowMinimum() {
        let out = Layout.shrinkToFit(Self.factory(), available: 100)
        for col in out { XCTAssertGreaterThanOrEqual(col.width, col.min, col.id) }
    }

    func testShrinkHidesNothing() {
        let out = Layout.shrinkToFit(Self.factory(), available: 100)
        XCTAssertTrue(out.allSatisfy { !$0.hidden })
    }

    func testShrinkLeavesAFittingLayoutByteIdentical() {
        let cols = Self.factory()
        XCTAssertEqual(Layout.shrinkToFit(cols, available: Layout.totalWidth(cols) + 1), cols)
    }

    func testFirstRunHidesPreviewThenAccountOnlyWhenNeeded() {
        let cols = Self.factory()
        // Roomy: nothing hidden.
        let wide = Layout.firstRunLayout(cols, available: 2000, narrowest: 2000)
        XCTAssertTrue(wide.allSatisfy { !$0.hidden })
        // Just enough with Preview at its minimum: shrink alone, nothing hidden.
        var atMin = cols
        atMin[atMin.firstIndex { $0.id == "preview" }!].width = cols.first { $0.id == "preview" }!.min
        let needsShrinkOnly = Layout.firstRunLayout(
            cols, available: Layout.totalWidth(atMin), narrowest: Layout.totalWidth(atMin))
        XCTAssertTrue(needsShrinkOnly.allSatisfy { !$0.hidden })
        // Tight: Preview goes first, Account still visible while that is enough.
        var noPreview = cols
        noPreview[noPreview.firstIndex { $0.id == "preview" }!].hidden = true
        // Narrowest this layout can be: every ELASTIC column at its minimum.
        let floorWidth = Layout.totalWidth(Layout.shrinkToFit(noPreview, available: 0))
        let tight = Layout.firstRunLayout(cols, available: floorWidth + 5, narrowest: floorWidth + 5)
        XCTAssertTrue(tight.first { $0.id == "preview" }!.hidden)
        XCTAssertFalse(tight.first { $0.id == "account" }!.hidden)
        // Tighter than that: Account goes too.
        let tiny = Layout.firstRunLayout(cols, available: 300)
        XCTAssertTrue(tiny.first { $0.id == "account" }!.hidden)
    }

    /// The extended hide order: Thread then Tags go after Preview and Account, only as
    /// space runs out, and a 500 pt pane (default 1200 pt window) ends with Received inside.
    func testFirstRunHideOrderExtendsToThreadAndTags() {
        XCTAssertEqual(Layout.hideOrder, ["preview", "account", "thread", "tags"])
        let cols = Self.factory()
        let out = Layout.firstRunLayout(cols, available: 500)
        XCTAssertEqual(out.filter(\.hidden).map(\.id).sorted(), ["account", "preview", "tags", "thread"])
        XCTAssertLessThanOrEqual(Layout.totalWidth(out), 500 + 0.01)
        XCTAssertFalse(out.first { $0.id == "date" }!.hidden)
        XCTAssertFalse(out.first { $0.id == "from" }!.hidden)
        XCTAssertFalse(out.first { $0.id == "subject" }!.hidden)
        // Thread survives while hiding Preview + Account is enough.
        let mid = Layout.firstRunLayout(cols, available: 560, narrowest: 560)
        XCTAssertFalse(mid.first { $0.id == "tags" }!.hidden)
    }

    func testHidingPreviewGivesSubjectItsRoomBack() {
        let cols = Self.factory()
        var noPreview = cols
        noPreview[noPreview.firstIndex { $0.id == "preview" }!].hidden = true
        let floorWidth = Layout.totalWidth(Layout.shrinkToFit(noPreview, available: 0))
        // Preview at its minimum would not fit (it costs min + overhead), but with it hidden
        // there is spare room: From must keep width above its minimum instead of the
        // "everything at the floor" result a shrink-then-hide on the shrunk columns gives.
        // The room comes back in reverse shrink order (Received, Account, then From), so
        // the spare is one point more than Received's and Account's slack.
        func slack(_ id: String) -> CGFloat { let c = cols.first { $0.id == id }!; return c.width - c.min }
        let spare = slack("date") + slack("account") + 1
        let preview = cols.first { $0.id == "preview" }!
        XCTAssertLessThan(spare, preview.min + Layout.perColumnOverhead, "precondition: Preview cannot fit")
        let avail = floorWidth + spare
        let out = Layout.firstRunLayout(cols, available: avail, narrowest: avail)
        XCTAssertTrue(out.first { $0.id == "preview" }!.hidden)
        XCTAssertEqual(width(out, "subject"), cols.first { $0.id == "subject" }!.min, accuracy: 0.01)
        XCTAssertGreaterThan(width(out, "from"), cols.first { $0.id == "from" }!.min,
                             "hiding Preview must give the room back to From")
        XCTAssertLessThanOrEqual(Layout.totalWidth(out), avail + 0.01)
    }

    /// Pins `perColumnOverhead` against a real inset NSTableView: the right edge of the
    /// last column is the sum of widths plus `overhead` per column.
    @MainActor
    func testOverheadMatchesARealTable() {
        let table = NSTableView()
        table.style = .inset
        MessageListView.applyColumnSpacing(table)
        table.columnAutoresizingStyle = .noColumnAutoresizing
        for spec in kColumns {
            let c = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(spec.id))
            c.width = spec.width; c.minWidth = spec.min; c.maxWidth = spec.max
            table.addTableColumn(c)
        }
        func measure() -> (right: CGFloat, sum: CGFloat, n: Int) {
            let visible = table.tableColumns.filter { !$0.isHidden }
            let last = table.tableColumns.lastIndex { !$0.isHidden }!
            return (table.rect(ofColumn: last).maxX, visible.reduce(0) { $0 + $1.width }, visible.count)
        }
        let all = measure()
        table.tableColumns.first { $0.identifier.rawValue == "preview" }!.isHidden = true
        table.tableColumns.first { $0.identifier.rawValue == "account" }!.isHidden = true
        let fewer = measure()
        // Two measurements, two unknowns: per-column overhead and the constant table overhead.
        let per = ((all.right - all.sum) - (fewer.right - fewer.sum)) / CGFloat(all.n - fewer.n)
        let constant = (all.right - all.sum) - CGFloat(all.n) * per
        XCTAssertEqual(per, Layout.perColumnOverhead, accuracy: 0.01,
                       "measured per-column overhead changed; update MessageListColumnLayout.perColumnOverhead")
        XCTAssertEqual(constant, Layout.tableOverhead, accuracy: 0.01,
                       "measured table overhead changed; update MessageListColumnLayout.tableOverhead")
        // And the model agrees with the table.
        let cols = Self.factory().map { c -> Layout.Column in
            var c = c; c.hidden = (c.id == "preview" || c.id == "account"); return c
        }
        XCTAssertEqual(Layout.totalWidth(cols), fewer.right, accuracy: 0.01)
    }

    // MARK: - #3042 follow-up: the list is squeezed to 420 pt when a message opens

    /// Selecting a message squeezes the Widescreen list to its 420 pt minimum at
    /// EVERY window size (measured 790 -> 420 at 1800 pt). A first-run layout made at
    /// a roomy width must still leave minimums that fit the narrowest pane with the
    /// Account column a unified list forces, so the shrink pass can always bring
    /// Received back inside.
    func testFirstRunPlansForTheSqueezedPaneAtAnyWidth() {
        for available: CGFloat in [500, 790, 1400] {
            let out = Layout.firstRunLayout(Self.factory(), available: available)
            XCTAssertLessThanOrEqual(Layout.totalWidth(out), available + 0.01, "fits the pane it saw (\(available))")
            XCTAssertLessThanOrEqual(Layout.minimumWidth(out, alsoShowing: ["account"]), Layout.narrowestPane + 0.01,
                                     "minimums with Account forced fit the narrowest pane (\(available))")
            let squeezed = Layout.shrinkToFit(out.map { var c = $0; if c.id == "account" { c.hidden = false }; return c },
                                              available: 420)
            XCTAssertLessThanOrEqual(Layout.totalWidth(squeezed), 420 + 0.01, "Received inside 420 pt (\(available))")
            for id in ["from", "subject", "date"] {
                XCTAssertFalse(out.first { $0.id == id }!.hidden, "\(id) visible (\(available))")
            }
        }
    }

    /// The numbers behind the plan: the first-run visible set with Account forced on
    /// costs 394 pt at its minimums, inside `narrowestPane` (420 less a legacy scroller).
    func testUnifiedMinimumsFitTheNarrowestPane() {
        let hidden: Set = ["preview", "thread", "tags"]
        let cols = Self.factory().map { c -> Layout.Column in var c = c; c.hidden = hidden.contains(c.id); return c }
        XCTAssertEqual(Layout.minimumWidth(cols), 394, accuracy: 0.01)
        XCTAssertLessThanOrEqual(Layout.minimumWidth(cols), Layout.narrowestPane)
    }

    /// Received gives up only its spare room, last, and never below the widest label.
    func testReceivedShrinksLastAndNotBelowItsMinimum() {
        XCTAssertEqual(Layout.shrinkOrder.last, "date")
        let out = Layout.shrinkToFit(Self.factory(), available: 0)
        XCTAssertEqual(width(out, "date"), kColumns.first { $0.id == "date" }!.min, accuracy: 0.01)
        XCTAssertGreaterThanOrEqual(kColumns.first { $0.id == "date" }!.min, 80, "holds '12 Jun 2024'")
    }

    /// Fit Columns: shrink first; hide (in `hideOrder`) only when even the minimums
    /// overflow, and never a column it was told to keep (the forced Account column).
    func testFitLayoutHidesOnlyAsALastResortAndKeepsTheForcedColumn() {
        let cols = Self.factory()
        // Roomy: same as shrinkToFit, nothing hidden.
        XCTAssertEqual(Layout.fitLayout(cols, available: 2000), Layout.shrinkToFit(cols, available: 2000))
        // Every column shown at 420 pt: hides until Received fits, Account kept.
        let out = Layout.fitLayout(cols, available: 420, keep: ["account"])
        XCTAssertLessThanOrEqual(Layout.totalWidth(out), 420 + 0.01)
        XCTAssertFalse(out.first { $0.id == "account" }!.hidden, "the forced Account column stays")
        XCTAssertTrue(out.first { $0.id == "preview" }!.hidden, "Preview goes first")
        for id in ["from", "subject", "date"] { XCTAssertFalse(out.first { $0.id == id }!.hidden, id) }
        // It stops hiding as soon as the row fits: Thread and Tags survive at 470 pt here.
        let roomier = Layout.fitLayout(cols, available: 470, keep: ["account"])
        XCTAssertLessThanOrEqual(Layout.totalWidth(roomier), 470 + 0.01)
        XCTAssertFalse(roomier.first { $0.id == "tags" }!.hidden)
    }
}
