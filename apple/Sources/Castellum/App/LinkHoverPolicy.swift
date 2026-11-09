// Castellum - link-hover reveal decision logic (S48).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure so it's testable without WKWebView.

import Foundation

enum LinkHoverPolicy {
    /// Runs the Rust inspection and decides what (if anything) the popover
    /// shows. `nil` `hover` means the pointer left the link — hide the popover.
    static func reveal(for hover: LinkHoverInfo?) -> (LinkHoverInfo, LinkInspection)? {
        guard let hover, !hover.href.isEmpty else { return nil }
        guard let inspection = try? CastellumCore.inspectLink(href: hover.href, visibleText: hover.text) else {
            return nil // never crash the reading pane over a malformed link
        }
        return (hover, inspection)
    }
}
