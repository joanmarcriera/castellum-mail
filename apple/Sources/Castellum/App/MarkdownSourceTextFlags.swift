// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// MarkdownSourceTextFlags.swift
//
// Keeps the compose body's Markdown source exactly as typed. macOS text views
// turn `---` into an em dash and straight quotes into curly ones, which breaks
// a table delimiter row (`|---|---|`) and horizontal rules, in the editor, the
// preview and on the wire.
//
// Usage: `TextEditor(text: $body).markdownSourceTyping(markdown: session.markdownEnabled)`.
// Markdown on  -> dash, quote and text-replacement substitution off.
// Markdown off -> quote and text-replacement go back to the system defaults; dash
//                 substitution stays off so a typed "-- " delimiter survives (RP02).
// Spelling correction is never touched.
import AppKit
import SwiftUI

enum MarkdownSourceTextFlags {
    /// The system defaults, as a freshly created NSTextView reads them from the user's settings.
    @MainActor static func systemDefaults() -> (dash: Bool, quote: Bool, replacement: Bool) {
        let fresh = NSTextView(frame: .zero)
        return (fresh.isAutomaticDashSubstitutionEnabled,
                fresh.isAutomaticQuoteSubstitutionEnabled,
                fresh.isAutomaticTextReplacementEnabled)
    }

    /// Apply the flags for the current Markdown mode to `textView`.
    @MainActor static func apply(markdown: Bool, to textView: NSTextView) {
        if markdown {
            textView.isAutomaticDashSubstitutionEnabled = false
            textView.isAutomaticQuoteSubstitutionEnabled = false
            textView.isAutomaticTextReplacementEnabled = false
        } else {
            let d = systemDefaults()
            // Never in a mail body: it turns a typed "-- " signature delimiter
            // into an em dash (RP02). Quotes and replacements keep the system setting.
            textView.isAutomaticDashSubstitutionEnabled = false
            textView.isAutomaticQuoteSubstitutionEnabled = d.quote
            textView.isAutomaticTextReplacementEnabled = d.replacement
        }
    }

    /// First editable NSTextView found in the nearest ancestor of `probe` that contains one.
    @MainActor static func nearestTextView(from probe: NSView) -> NSTextView? {
        func find(_ v: NSView) -> NSTextView? {
            if let t = v as? NSTextView, !t.isFieldEditor, t.isEditable { return t }
            for s in v.subviews { if let t = find(s) { return t } }
            return nil
        }
        var cur: NSView? = probe.superview
        while let c = cur {
            if let t = find(c) { return t }
            cur = c.superview
        }
        return nil
    }
}

/// Zero-size probe that finds the TextEditor's NSTextView and keeps its flags in step with `markdown`.
private struct MarkdownSourceProbe: NSViewRepresentable {
    let markdown: Bool

    func makeNSView(context: Context) -> NSView { NSView(frame: .zero) }

    func updateNSView(_ view: NSView, context: Context) {
        let markdown = markdown
        // The text view may not be in the hierarchy yet on the first pass; apply now and once more next turn.
        func attempt() {
            if let tv = MarkdownSourceTextFlags.nearestTextView(from: view) {
                MarkdownSourceTextFlags.apply(markdown: markdown, to: tv)
            }
        }
        attempt()
        DispatchQueue.main.async { attempt() }
    }
}

extension View {
    /// Keep a TextEditor's Markdown source free of smart dashes, quotes and text replacement.
    func markdownSourceTyping(markdown: Bool) -> some View {
        background(MarkdownSourceProbe(markdown: markdown).frame(width: 0, height: 0))
    }
}
