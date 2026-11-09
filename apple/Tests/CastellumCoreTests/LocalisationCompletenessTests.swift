// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// L12 / FP50 guard: every user-visible literal handed to a SwiftUI localising initialiser must
/// have a translated `es` and `ca` unit in Localizable.xcstrings, so Settings and friends never
/// ship half English again (the catalogue was frozen at S60 with no check).
///
/// The scan is a regex over the source tree (read through `#filePath`, like
/// FilterGroupModelTests) because SwiftUI gives no runtime list of the literals it was asked to
/// localise. Interpolations (`\(…)`, nested quotes included) normalise to `%@`, and `%lld`/`%d`
/// in catalogue keys normalise the same way, so call sites and keys compare equal.
final class LocalisationCompletenessTests: XCTestCase {
    // MARK: Source tree + catalogue

    private static let root = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()

    /// Literals that are not prose: units, example addresses.
    private static let allowlist: Set<String> = [
        "MB", "replies@example.com", "archive@example.com",
    ]

    /// Normalised catalogue key -> set of languages that have a non-empty translation.
    private static func catalogue() throws -> [String: Set<String>] {
        let url = root.appendingPathComponent("Sources/Castellum/Resources/Localizable.xcstrings")
        let obj = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        let strings = obj["strings"] as! [String: [String: Any]]
        var out: [String: Set<String>] = [:]
        for (key, entry) in strings {
            let locs = entry["localizations"] as? [String: [String: Any]] ?? [:]
            var langs = Set<String>()
            for (lang, unit) in locs {
                if let su = unit["stringUnit"] as? [String: Any],
                   let v = su["value"] as? String, !v.isEmpty { langs.insert(lang) }
                if unit["variations"] != nil { langs.insert(lang) }
            }
            out[normalise(key), default: []].formUnion(langs)
        }
        return out
    }

    private static func normalise(_ key: String) -> String {
        key.replacingOccurrences(
            of: #"%(\d+\$)?(lld|ld|d|@|lf|f)"#, with: "%@", options: .regularExpression)
    }

    // MARK: Literal scanner

    private static let call = try! NSRegularExpression(pattern:
        #"(?:\b(?:Text|Button|Toggle|Picker|Section|SettingsSection|SettingsRow|Label|LabeledContent|TextField|SecureField|Menu|Tab|DisclosureGroup|GroupBox|Link|Stepper)\(|\.(?:help|navigationTitle|alert|confirmationDialog|accessibilityLabel|accessibilityHint)\(|String\(localized:\s*|LocalizedStringKey\()\s*""#)

    /// Reads the literal starting just after its opening quote; `\(…)` (with nested strings and
    /// parentheses) becomes `%@`, `\u{…}` and the common escapes are decoded. nil if unterminated.
    static func literal(_ line: [Character], from start: Int) -> String? {
        var out = "", i = start
        while i < line.count {
            let c = line[i]
            if c == "\"" { return out }
            guard c == "\\", i + 1 < line.count else { out.append(c); i += 1; continue }
            let d = line[i + 1]
            switch d {
            case "(":
                var depth = 1, j = i + 2
                while j < line.count, depth > 0 {
                    if line[j] == "\"" {
                        j += 1
                        while j < line.count, line[j] != "\"" { j += line[j] == "\\" ? 2 : 1 }
                    } else if line[j] == "(" { depth += 1 } else if line[j] == ")" { depth -= 1 }
                    j += 1
                }
                out += "%@"; i = j
            case "u":
                guard let close = line[i...].firstIndex(of: "}"), i + 3 <= close,
                      let v = UInt32(String(line[(i + 3)..<close]), radix: 16),
                      let scalar = Unicode.Scalar(v) else { out.append(d); i += 2; continue }
                out.unicodeScalars.append(scalar); i = close + 1
            case "n": out += "\n"; i += 2
            case "t": out += "\t"; i += 2
            default: out.append(d); i += 2
            }
        }
        return nil
    }

    /// Every (file:line, normalised literal) the app source asks SwiftUI to localise.
    private static func literals() throws -> [(String, String)] {
        let dir = root.appendingPathComponent("Sources/Castellum/App")
        var found: [(String, String)] = []
        let files = FileManager.default.enumerator(at: dir, includingPropertiesForKeys: nil)!
            .compactMap { $0 as? URL }
            .filter { $0.pathExtension == "swift" && !$0.path.contains("UIHarness") }
            .sorted { $0.path < $1.path }
        for file in files {
            let text = try String(contentsOf: file, encoding: .utf8)
            for (n, line) in text.components(separatedBy: "\n").enumerated() {
                if line.trimmingCharacters(in: .whitespaces).hasPrefix("//") { continue }
                let chars = Array(line)
                let ns = line as NSString
                for m in call.matches(in: line, range: NSRange(location: 0, length: ns.length)) {
                    // NSRange is UTF-16; map back to a Character index.
                    let prefix = ns.substring(to: m.range.location + m.range.length)
                    guard let s = literal(chars, from: prefix.count) else { continue }
                    // Pure punctuation / placeholders (e.g. "%@: ", "%@%") need no translation.
                    if s.replacingOccurrences(of: "%@", with: "")
                        .range(of: "[A-Za-z]{2}", options: .regularExpression) == nil { continue }
                    found.append(("\(file.lastPathComponent):\(n + 1)", s))
                }
            }
        }
        return found
    }

    // MARK: Tests

    func testEveryLocalisingLiteralHasCatalanAndSpanish() throws {
        let cat = try Self.catalogue()
        var missing: [String] = []
        for (where_, s) in try Self.literals() where !Self.allowlist.contains(s) {
            let have = cat[Self.normalise(s)] ?? []
            if !have.isSuperset(of: ["es", "ca"]) { missing.append("\(where_): \"\(s)\"") }
        }
        XCTAssertTrue(missing.isEmpty,
                      "\(missing.count) literal(s) lack es/ca in Localizable.xcstrings:\n"
                      + missing.joined(separator: "\n"))
    }

    /// Enum `label`s are plain `String`s fed to `Text(verbatim)`-like pickers, so they only
    /// translate when built with `String(localized:)`; the English text must be a catalogue key.
    func testEnumLabelsAreLocalised() throws {
        let cat = try Self.catalogue()
        var labels: [String] = []
        labels += AutoMarkReadMode.allCases.map(\.label)
        labels += UpdateCheckSchedule.allCases.map(\.label)
        labels += UpdateCheckSchedule.allCases.map(\.disclosure)
        labels += NewMailNotifications.allCases.map(\.label)
        labels += DockBadgeMode.allCases.map(\.label)
        labels.append(UndoSendDelay.off.label)
        for l in labels {
            let have = cat[Self.normalise(l)] ?? []
            XCTAssertTrue(have.isSuperset(of: ["en", "es", "ca"]) || have.isSuperset(of: ["es", "ca"]),
                          "enum label \"\(l)\" has no es/ca entry")
        }
    }
}
