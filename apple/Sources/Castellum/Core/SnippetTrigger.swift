// Castellum - inline `;abbrev` + Tab expansion (MM-7, slice 3).
//
// Pure: finds the abbreviation just before the caret. The key handling that calls this
// lives in `ComposeBodyAccess.swift` and is OFF by default (`TemplateLibrary.inlineTriggerEnabled`):
// Tab is also how people leave a field, so it is only taken when a snippet really matches.
//
// Rules: the `;` must start the text or follow whitespace (so `a;b` and URLs with `;`
// never trigger); the abbreviation is `[a-z0-9-]`, up to 24 characters, matched ignoring
// case; the longest run of such characters directly before the caret is the candidate.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum SnippetTrigger {
    /// The snippet named by a `;abbrev` ending `textBeforeCaret`, and the UTF-16 range of that
    /// token (the `;` included) to replace. `items` must already be scoped to the account.
    static func match(textBeforeCaret: String, in items: [SnippetItem]) -> (replace: NSRange, item: SnippetItem)? {
        let scalars = Array(textBeforeCaret.unicodeScalars)
        // Walk back over abbreviation characters to a `;`.
        var i = scalars.count
        while i > 0, isAbbreviationScalar(scalars[i - 1]) { i -= 1 }
        guard i > 0, scalars[i - 1] == ";" else { return nil }
        let semicolon = i - 1
        // The `;` must start the text or follow whitespace.
        if semicolon > 0, !isWhitespace(scalars[semicolon - 1]) { return nil }
        let abbreviation = String(String.UnicodeScalarView(scalars[i...]))
        guard !abbreviation.isEmpty, abbreviation.count <= SnippetItem.maxAbbreviationLength,
              let item = items.first(where: { $0.abbreviation?.lowercased() == abbreviation.lowercased() })
        else { return nil }
        let location = String(String.UnicodeScalarView(scalars[..<semicolon])).utf16.count
        return (NSRange(location: location, length: 1 + abbreviation.utf16.count), item)
    }

    private static func isAbbreviationScalar(_ s: Unicode.Scalar) -> Bool {
        ("a"..."z").contains(s) || ("A"..."Z").contains(s) || ("0"..."9").contains(s) || s == "-"
    }

    private static func isWhitespace(_ s: Unicode.Scalar) -> Bool {
        s.properties.isWhitespace
    }
}
