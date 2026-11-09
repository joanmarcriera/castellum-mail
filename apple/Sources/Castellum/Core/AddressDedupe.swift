// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import Foundation

/// Recipient de-duplication for outgoing mail (FP07).
///
/// Usage: `AddressDedupe.dedupe(to:cc:bcc:)` returns the three lists with
/// every address kept once, comparing the lowercased email across To, then
/// Cc, then Bcc (first occurrence wins, so an address in To is dropped from
/// Cc/Bcc). When a later duplicate carries a display name the first lacks,
/// the name is adopted so "Bob Ferrer <bob@x>" is not lost to a bare "bob@x".
enum AddressDedupe {
    static func dedupe(
        to: [DraftAddress], cc: [DraftAddress], bcc: [DraftAddress]
    ) -> (to: [DraftAddress], cc: [DraftAddress], bcc: [DraftAddress]) {
        // Pass 1: where each address first appears, and the best name seen for it.
        var bestName: [String: String] = [:]
        for a in to + cc + bcc {
            let key = a.email.lowercased()
            if bestName[key] == nil, let n = a.name, !n.trimmingCharacters(in: .whitespaces).isEmpty {
                bestName[key] = n
            }
        }
        var seen = Set<String>()
        func pass(_ list: [DraftAddress]) -> [DraftAddress] {
            var out: [DraftAddress] = []
            for a in list {
                let key = a.email.lowercased()
                guard seen.insert(key).inserted else { continue }
                var kept = a
                if kept.name?.trimmingCharacters(in: .whitespaces).isEmpty ?? true {
                    kept.name = bestName[key]
                }
                out.append(kept)
            }
            return out
        }
        let t = pass(to), c = pass(cc), b = pass(bcc)
        return (t, c, b)
    }
}
