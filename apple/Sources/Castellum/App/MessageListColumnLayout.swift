// Castellum - pane-aware column layout for the message list (#3042).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Why this exists: the factory column widths summed to more than the list pane
// (709 pt in Widescreen, 1069 pt in Classic), so the Received column — the
// right-most one — started off-screen and a new user saw no dates until they
// scrolled sideways. These pure functions shrink the elastic columns (Preview,
// then Subject, From, Account and Received's spare room) toward their minimums
// until the whole row fits, and, for a brand-new install or an explicit Fit
// Columns, hide Preview, Account, Thread, Tags (in that order) when even the
// minimums cannot fit. A new install also plans for the narrowest pane (the
// list is squeezed to its 420 pt minimum when a message opens), so the dates
// stay visible after the first click, not only before it.
//
// Pure and AppKit-free apart from `CGFloat`, so the arithmetic is unit-tested
// without a window. `MessageListView.Coordinator` maps `NSTableColumn`s to
// `Column`s, calls these, and writes the widths back.

import CoreGraphics

enum MessageListColumnLayout {
    struct Column: Equatable {
        let id: String
        var width: CGFloat
        let min: CGFloat
        let max: CGFloat
        var hidden: Bool
    }

    /// Horizontal gap between columns. AppKit's default is 17 pt, which cost 119 pt of a
    /// seven-column row (28% of the 420 pt narrowest list pane) and was the main reason
    /// Received could not fit (#3042). `MessageListView.applyColumnSpacing` sets it.
    static let intercellWidth: CGFloat = 8

    /// Width one visible column costs beyond its own width: with the `.inset` style
    /// this is exactly the intercell spacing. Measured from a real `NSTableView`
    /// (with `tableOverhead`) by
    /// `MessageListColumnLayoutTests.testOverheadMatchesARealTable`, which fails
    /// if AppKit ever changes it.
    static let perColumnOverhead: CGFloat = 8

    /// Elastic columns, in the order they give up width. `account` is late so
    /// the unified list (which force-shows it) keeps it readable; `date` is last
    /// and only gives up its spare room above the widest label ("12 Jun 2024").
    static let shrinkOrder = ["preview", "subject", "from", "account", "date"]

    /// Narrowest list pane Received must fit in: the Widescreen list's minimum width
    /// (420 pt, `RootView`) less a legacy (always-visible) scroller. Selecting a message
    /// squeezes the list to that minimum, so a first-run layout that only fits the
    /// pane it first sees (500 pt at the default window) clips Received as soon as a
    /// message opens. `firstRunLayout` keeps the visible minimums, Account included
    /// (a unified list force-shows it), inside this width.
    static let narrowestPane: CGFloat = 404

    /// Columns a first-run layout (and, as a last resort, Fit Columns) hides, in order,
    /// when the minimums still do not fit. With all four hidden, and Account forced
    /// back by a unified list, the minimums total 394 pt, inside `narrowestPane`.
    /// The responsive pass on a pane resize and M1 only shrink; they hide nothing.
    static let hideOrder = ["preview", "account", "thread", "tags"]

    /// Constant extra width of the whole table (the `.inset` style's leading/trailing
    /// padding), independent of the column count. Measured with `perColumnOverhead`
    /// (AppKit: 22 pt less the intercell width).
    static let tableOverhead: CGFloat = 14

    /// Rendered row width: every visible column's width plus its overhead.
    static func totalWidth(_ columns: [Column]) -> CGFloat {
        let visible = columns.filter { !$0.hidden }
        guard !visible.isEmpty else { return 0 }
        return visible.reduce(tableOverhead) { $0 + $1.width + perColumnOverhead }
    }

    /// Shrinks the elastic columns toward their minimums until the total fits
    /// `available`. Never goes below a minimum and hides nothing; if the
    /// minimums alone do not fit, it returns the minimums.
    static func shrinkToFit(_ columns: [Column], available: CGFloat) -> [Column] {
        var out = columns
        var excess = totalWidth(out) - available
        guard excess > 0 else { return out }
        for id in shrinkOrder {
            guard excess > 0, let i = out.firstIndex(where: { $0.id == id && !$0.hidden }) else { continue }
            let give = Swift.min(excess, Swift.max(0, out[i].width - out[i].min))
            out[i].width -= give
            excess -= give
        }
        return out
    }

    /// Narrowest row the shrink pass can reach: every elastic column at its minimum.
    /// `alsoShowing` counts columns as visible even if hidden now (Account, which a
    /// unified list force-shows after the layout ran).
    static func minimumWidth(_ columns: [Column], alsoShowing: Set<String> = []) -> CGFloat {
        let shown = columns.map { c -> Column in
            var c = c
            if alsoShowing.contains(c.id) { c.hidden = false }
            return c
        }
        return totalWidth(shrinkToFit(shown, available: 0))
    }

    /// New-install layout: `shrinkToFit`, then hides `hideOrder` columns while the
    /// row does not fit `available`, OR while its minimums (Account counted) would
    /// not fit the narrower of `available` and `narrowest` (re-shrinking after each
    /// hide). The second rule is what keeps Received on screen once a message is
    /// selected and the split view squeezes the list to its minimum width (#3042).
    static func firstRunLayout(_ columns: [Column], available: CGFloat,
                               narrowest: CGFloat = narrowestPane) -> [Column] {
        let floor = Swift.min(available, narrowest)
        // Recompute from the ORIGINAL widths after each hide, so hiding Preview
        // gives Subject and From their room back instead of leaving them at
        // their minimums.
        var base = columns
        var out = shrinkToFit(base, available: available)
        func tooWide() -> Bool {
            totalWidth(out) > available || minimumWidth(base, alsoShowing: ["account"]) > floor
        }
        for id in hideOrder where tooWide() {
            guard let i = base.firstIndex(where: { $0.id == id }) else { continue }
            base[i].hidden = true
            out = shrinkToFit(base, available: available)
        }
        return out
    }

    /// View > Fit Columns, after every column was auto-fitted to its content:
    /// shrink to `available`; if even the minimums overflow, hide `hideOrder`
    /// columns (never one in `keep`, such as the Account column a unified list
    /// forces) until Received is back inside the pane. The user asked for a fit,
    /// so a fit is what they get; the header menu re-shows a column.
    static func fitLayout(_ columns: [Column], available: CGFloat, keep: Set<String> = []) -> [Column] {
        var out = shrinkToFit(columns, available: available)
        for id in hideOrder where totalWidth(out) > available && !keep.contains(id) {
            guard let i = out.firstIndex(where: { $0.id == id && !$0.hidden }) else { continue }
            out[i].hidden = true
            out = shrinkToFit(out, available: available)
        }
        return out
    }
}
