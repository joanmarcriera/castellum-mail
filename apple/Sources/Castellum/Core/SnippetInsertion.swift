// Castellum - where and how a rendered snippet lands in a compose body (MM-7, #2697).
//
// Pure: no UI. The live path inserts through the body's NSTextView (native undo,
// keeps the SwiftUI binding in step) and uses `insertionRange` to decide WHERE;
// `apply` is the same decision applied to a plain string, the fallback when the
// text view cannot be found, and what the unit tests drive.
//
// Rule: the selection is replaced; a selection or caret that reaches into the
// signature block or the quoted reply is clamped to the end of the text the user
// wrote (`SignatureSupport.authoredRegionEnd`), so a snippet never lands inside a quote.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum SnippetInsertion {
    /// The range (UTF-16, as NSTextView counts) that inserting replaces.
    static func insertionRange(in body: String, selection: NSRange) -> NSRange {
        let ns = body as NSString
        let limit = SignatureSupport.authoredRegionEnd(in: body).utf16Offset(in: body)
        guard selection.location != NSNotFound, selection.location <= ns.length else {
            return NSRange(location: limit, length: 0)   // no usable selection: the end of the authored text
        }
        if selection.location > limit { return NSRange(location: limit, length: 0) }
        let end = min(selection.location + selection.length, limit)
        return NSRange(location: selection.location, length: end - selection.location)
    }

    /// `rendered` inserted into `body` at `selection`; returns the new body and the caret
    /// (UTF-16 offset): at the snippet's `${cursor}`, else just after the inserted text.
    static func apply(_ rendered: RenderedTemplate, to body: String, selection: NSRange) -> (body: String, caret: Int) {
        let range = insertionRange(in: body, selection: selection)
        let result = (body as NSString).replacingCharacters(in: range, with: rendered.text)
        let caret = range.location + (rendered.caretOffset ?? rendered.text.utf16.count)
        return (result, caret)
    }
}
