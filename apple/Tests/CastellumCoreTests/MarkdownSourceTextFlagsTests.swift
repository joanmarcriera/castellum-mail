// Castellum - F2: the compose body keeps Markdown source as typed (no smart dashes/quotes).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI
import XCTest
@testable import Castellum

@MainActor
final class MarkdownSourceTextFlagsTests: XCTestCase {
    /// Windows kept alive for the test's duration; never ordered front.
    private var windows: [NSWindow] = []

    /// Substitution only happens for a view that lives in a window and is its
    /// first responder, so host the view in an (offscreen, never-shown)
    /// window; on a windowless view `insertText` is a plain insert and the
    /// "stays literal" assertions would pass vacuously.
    private func editableView() -> NSTextView {
        let tv = NSTextView(frame: NSRect(x: 0, y: 0, width: 300, height: 100))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 300, height: 100),
                              styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = tv
        window.makeFirstResponder(tv)
        windows.append(window)
        tv.isEditable = true
        // Start from the aggressive state a user with smart typing on would have.
        tv.isAutomaticDashSubstitutionEnabled = true
        tv.isAutomaticQuoteSubstitutionEnabled = true
        tv.isAutomaticTextReplacementEnabled = true
        return tv
    }

    func testMarkdownOnKeepsDelimiterRowRuleAndStraightQuotes() {
        let tv = editableView()
        MarkdownSourceTextFlags.apply(markdown: true, to: tv)
        XCTAssertFalse(tv.isAutomaticDashSubstitutionEnabled)
        XCTAssertFalse(tv.isAutomaticQuoteSubstitutionEnabled)
        XCTAssertFalse(tv.isAutomaticTextReplacementEnabled)
        // Control: with the flags left ON the same text IS rewritten, proving
        // this harness really exercises substitution.
        let control = editableView()
        control.insertText("\"quoted\"", replacementRange: NSRange(location: 0, length: 0))
        control.checkTextInDocument(nil)
        XCTAssertNotEqual(control.string, "\"quoted\"", "harness: smart quotes should rewrite here")
        for s in ["|---|---|", "---", "\"quoted\" and 'single'"] {
            tv.string = ""
            tv.insertText(s, replacementRange: NSRange(location: 0, length: 0))
            tv.checkTextInDocument(nil)   // the pass that applies enabled substitutions
            XCTAssertEqual(tv.string, s)
        }
    }

    func testMarkdownOffRestoresSystemDefaults() {
        let tv = editableView()
        MarkdownSourceTextFlags.apply(markdown: true, to: tv)
        MarkdownSourceTextFlags.apply(markdown: false, to: tv)
        let d = MarkdownSourceTextFlags.systemDefaults()
        XCTAssertFalse(tv.isAutomaticDashSubstitutionEnabled, "a typed \"-- \" delimiter must survive (RP02)")
        XCTAssertEqual(tv.isAutomaticQuoteSubstitutionEnabled, d.quote)
        XCTAssertEqual(tv.isAutomaticTextReplacementEnabled, d.replacement)
    }

    func testSpellingCorrectionIsLeftAlone() {
        let tv = editableView()
        tv.isAutomaticSpellingCorrectionEnabled = true
        MarkdownSourceTextFlags.apply(markdown: true, to: tv)
        XCTAssertTrue(tv.isAutomaticSpellingCorrectionEnabled)
    }

    /// The real TextEditor: toggling Markdown on an open window applies at once.
    func testTogglingMarkdownOnAnOpenTextEditorAppliesImmediately() async throws {
        final class Box: ObservableObject { @Published var md = false; @Published var text = "" }
        struct Host: View {
            @ObservedObject var box: Box
            var body: some View {
                TextEditor(text: $box.text).markdownSourceTyping(markdown: box.md).frame(width: 300, height: 100)
            }
        }
        let box = Box()
        let hosting = NSHostingView(rootView: Host(box: box))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 300, height: 100),
                              styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        func tv() -> NSTextView? {
            func find(_ v: NSView) -> NSTextView? {
                if let t = v as? NSTextView { return t }
                for s in v.subviews { if let t = find(s) { return t } }
                return nil
            }
            return find(hosting)
        }
        func settle() async { try? await Task.sleep(nanoseconds: 300_000_000); hosting.layoutSubtreeIfNeeded() }
        await settle()
        let defaults = MarkdownSourceTextFlags.systemDefaults()
        let view = try XCTUnwrap(tv())
        XCTAssertFalse(view.isAutomaticDashSubstitutionEnabled, "plain mode never turns -- into an em dash (RP02)")
        box.md = true
        await settle()
        XCTAssertFalse(try XCTUnwrap(tv()).isAutomaticDashSubstitutionEnabled)
        XCTAssertFalse(try XCTUnwrap(tv()).isAutomaticQuoteSubstitutionEnabled)
        box.md = false
        await settle()
        XCTAssertFalse(try XCTUnwrap(tv()).isAutomaticDashSubstitutionEnabled)
        XCTAssertEqual(try XCTUnwrap(tv()).isAutomaticQuoteSubstitutionEnabled, defaults.quote)
    }
}
