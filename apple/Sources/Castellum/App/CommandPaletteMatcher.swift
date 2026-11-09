// Castellum - pure fuzzy matcher for the command palette (S45). No AppKit/
// SwiftUI import: scoring a query against a command title is ordinary Swift
// string logic, independent of how the palette presents results.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

enum CommandPaletteMatcher {
    /// Case-insensitive subsequence fuzzy match: every character of `query`
    /// must appear in `target`, in order, gaps allowed. `nil` = no match.
    /// Higher score = better: a character matched immediately after the
    /// previous match (a consecutive run) or at a word boundary (the very
    /// start of `target`, or right after a space) scores more than one
    /// found deep inside a word — the same shape VS Code's/Sublime's palette
    /// matchers use, reimplemented here with no dependency. An empty query
    /// matches everything with score 0, so the palette shows the full
    /// registry order when first opened.
    static func score(query: String, target: String) -> Int? {
        guard !query.isEmpty else { return 0 }
        let q = Array(query.lowercased())
        let t = Array(target.lowercased())
        var qi = 0
        var total = 0
        var previousMatchIndex = -2
        for (ti, ch) in t.enumerated() {
            guard qi < q.count, ch == q[qi] else { continue }
            var gain = 1
            if ti == previousMatchIndex + 1 { gain += 8 }
            if ti == 0 || t[ti - 1] == " " { gain += 4 }
            total += gain
            previousMatchIndex = ti
            qi += 1
        }
        return qi == q.count ? total : nil
    }

    /// Ranks `commands` against `query`, highest score first. Swift's `sorted`
    /// is stable, so ties keep the registry's own order — a deliberate,
    /// deterministic tiebreak rather than an arbitrary one.
    static func rank(query: String, commands: [CommandSpec]) -> [CommandSpec] {
        commands
            .compactMap { spec -> (CommandSpec, Int)? in
                // The palette row renders `spec.help ?? spec.title`, so the
                // text the user can SEE must be searchable; the menu title
                // stays searchable too (best of the two) for users who know
                // the command by its menu name.
                let shown = score(query: query, target: spec.help ?? spec.title)
                let title = spec.help == nil ? nil : score(query: query, target: spec.title)
                guard let s = [shown, title].compactMap({ $0 }).max() else { return nil }
                return (spec, s)
            }
            .sorted { $0.1 > $1.1 }
            .map(\.0)
    }
}
