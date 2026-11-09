// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest

/// L12 / FP51: count strings need plural variations in en, es and ca, otherwise the UI says
/// "1 messages" / "1 actions queued". Call sites must pass the Int itself (key `%lld`), because a
/// pre-formatted `%@` string cannot drive plural selection.
final class LocalisationPluralTests: XCTestCase {
    private func strings() throws -> [String: [String: Any]] {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/Castellum/Resources/Localizable.xcstrings")
        let root = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        return root["strings"] as! [String: [String: Any]]
    }

    /// `variations.plural` directly on the localisation, or on any of its `substitutions`
    /// (multi-argument keys), must carry both `one` and `other`.
    private func hasOneAndOther(_ loc: [String: Any]) -> Bool {
        func ok(_ v: [String: Any]?) -> Bool {
            guard let p = (v?["variations"] as? [String: Any])?["plural"] as? [String: Any] else { return false }
            return p["one"] != nil && p["other"] != nil
        }
        if ok(loc) { return true }
        let subs = loc["substitutions"] as? [String: [String: Any]] ?? [:]
        return !subs.isEmpty && subs.values.contains(where: { ok($0) })
    }

    func testCountStringsHavePluralVariations() throws {
        let re = try NSRegularExpression(pattern: #"%(lld|@) (messages|actions|seconds|days|signatures|attempts|folders)"#)
        var bad: [String] = []
        var seen = 0
        for (key, entry) in try strings() {
            guard re.firstMatch(in: key, range: NSRange(key.startIndex..., in: key)) != nil else { continue }
            seen += 1
            let locs = entry["localizations"] as? [String: [String: Any]] ?? [:]
            for lang in ["en", "es", "ca"] where !(locs[lang].map(hasOneAndOther) ?? false) {
                bad.append("\(lang): \(key)")
            }
        }
        XCTAssertGreaterThan(seen, 10, "scanner found too few count strings")
        XCTAssertTrue(bad.isEmpty, "count strings without plural one/other:\n" + bad.sorted().joined(separator: "\n"))
    }

    func testNoFormattedCountKeysRemain() throws {
        // A `%@` count key means a call site pre-formats the number; plural selection cannot work.
        let bad = try strings().keys.filter {
            $0.range(of: #"%@ (messages|actions|seconds|signatures) "#, options: .regularExpression) != nil
                || $0.hasSuffix("%@ messages") || $0.hasSuffix("%@ seconds")
        }
        XCTAssertTrue(bad.isEmpty, "pre-formatted count keys: \(bad)")
    }
}
