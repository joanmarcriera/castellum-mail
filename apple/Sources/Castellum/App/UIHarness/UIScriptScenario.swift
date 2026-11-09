// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 UI-script scenario schema (Debug builds only).
//
// Usage: a scenario is a JSON file (scripts/ui-scenarios/*.json) named by the
// `CASTELLUM_UI_SCRIPT` env var when `scripts/ui-check.sh` launches the app.
// Schema and ops: see the ops table below, DECISIONS.md (UIT-1) and
// .agents/skills/castellum-native-verify/SKILL.md layer 5a. Validation happens BEFORE any step runs, so a
// typo in an op or target fails fast (and fails UIScriptScenarioTests in CI).
#if DEBUG
import Foundation

struct UIScenario: Codable {
    struct Requires: Codable {
        var greenmail: Bool?
        var personas: Bool?
    }
    struct Screen: Codable {
        var width: Double
        var height: Double
        var menuBar: Double?
    }
    var schema: Int
    var name: String
    var covers: [String]?
    var requires: Requires?
    var screen: Screen?
    var timeoutMs: Int?
    /// Targets that may legitimately be absent on this branch; a scenario using one that is
    /// missing SKIPS (exit 3, loud, non-zero unless --allow-skip). Any other unknown target is invalid.
    var optionalTargets: [String]?
    var steps: [UIStep]
}

/// `contentSize` is `[w, h]`, or the strings "min" / "ideal".
enum UISizeSpec: Codable, Equatable {
    case size(Double, Double), min, ideal

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let a = try? c.decode([Double].self), a.count == 2 { self = .size(a[0], a[1]); return }
        let s = try c.decode(String.self)
        switch s {
        case "min": self = .min
        case "ideal": self = .ideal
        default: throw DecodingError.dataCorruptedError(in: c, debugDescription: "contentSize must be [w,h], \"min\" or \"ideal\"")
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .size(let w, let h): try c.encode([w, h])
        case .min: try c.encode("min")
        case .ideal: try c.encode("ideal")
        }
    }
}

struct UIFind: Codable, Equatable {
    var identifier: String?
    var label: String?
    var role: String?
    var text: String?   // substring of a visible text/label value
    /// Which of several visible matches a verb acts on (0-based, top of the window first within
    /// each kind); for controls that share a label, e.g. the two inline "Test" buttons.
    var index: Int? = nil
}

struct UIExpect: Codable {
    var find: UIFind?
    var visible: Bool?
    /// true = the control lies fully inside every enclosing scroll clip view (and the window), i.e. is on screen without scrolling.
    var inVisibleRect: Bool?
    var enabled: Bool?
    var exists: Bool?
    var windowCount: Int?
    var sheetAttached: Bool?
    var model: String?       // "accountSync:<ref>" | "errorText" | "accountCount"
    var equals: String?
}

/// One scenario step. A flat struct (rather than an enum per op) keeps the JSON
/// forgiving to write; `UIScenarioValidator` checks the per-op required fields.
struct UIStep: Codable {
    var op: String
    var label: String?
    var timeoutMs: Int?
    // wait
    var until: String?
    // addAccount
    var persona: String?
    var email: String?
    var password: String?
    var imap: String?
    var smtp: String?
    var tls: Bool?
    var `as`: String?
    var waitFor: String?
    // openWindow / openSheet
    var target: String?
    var tab: String?
    var accountRef: String?
    // command / search / setText
    var id: String?
    var allowServerWrites: Bool?
    var text: String?
    // window ops
    var window: String?
    var contentSize: UISizeSpec?
    var find: UIFind?
    var value: Bool?
    // capture / assert
    var name: String?
    var windows: [String]?
    var rules: [String]?
    var expect: [UIExpect]?
}

enum UIScenarioValidator {
    static let ops: Set<String> = [
        "wait", "addAccount", "openWindow", "openSheet", "command", "search", "resizeWindow",
        "press", "setText", "toggle", "closeWindow", "dismissSheet", "capture", "assert", "quit",
        "reading", "accounts",
        "ext",
    ]
    static let waitPrefixes = ["window:", "sheet:", "noSheet:", "sync:"]
    static let waitKeywords: Set<String> = ["idle", "accountsLoaded", "settingsOpenerReady"]
    static let windowTargets: Set<String> = [
        "settings", "contacts", "diagnostics", "outbox", "status", "aiActivity", "projects", "compose", "signatures",
    ]
    static let sheetTargets: Set<String> = ["addAccount", "saveSearch"]
    static let personas: Set<String> = ["alice", "bob", "carol", "dave"]

    /// Targets an `optionalTargets` scenario uses that this branch's registry lacks (skip, exit 3).
    static func missingOptionalTargets(in s: UIScenario) -> [String] {
        let optional = Set(s.optionalTargets ?? [])
        return s.steps.compactMap { st -> String? in
            guard let t = st.target, optional.contains(t) else { return nil }
            switch st.op {
            case "openWindow": return windowTargets.contains(t) ? nil : t
            case "openSheet": return sheetTargets.contains(t) ? nil : t
            default: return nil
            }
        }
    }

    /// Returns human-readable problems; empty = valid. An unknown target is a problem (a typo
    /// must not pass) unless the scenario lists it in `optionalTargets` (see `missingOptionalTargets`).
    static func problems(in s: UIScenario) -> [String] {
        var out: [String] = []
        if s.schema != 1 { out.append("unsupported schema \(s.schema)") }
        if s.name.isEmpty { out.append("empty name") }
        if s.steps.isEmpty { out.append("no steps") }
        for (i, step) in s.steps.enumerated() {
            let at = "step \(i + 1) (\(step.op))"
            guard ops.contains(step.op) else { out.append("\(at): unknown op"); continue }
            switch step.op {
            case "wait":
                guard let u = step.until else { out.append("\(at): missing until"); break }
                if !waitKeywords.contains(u) && !waitPrefixes.contains(where: { u.hasPrefix($0) }) {
                    out.append("\(at): unknown until '\(u)'")
                }
            case "addAccount":
                if step.persona == nil && (step.email == nil || step.password == nil) {
                    out.append("\(at): needs persona or email+password")
                }
                if let p = step.persona, !personas.contains(p) { out.append("\(at): unknown persona '\(p)'") }
                if let w = step.waitFor, !["saved", "synced", "none"].contains(w) { out.append("\(at): bad waitFor '\(w)'") }
            case "openWindow":
                if !windowTargets.contains(step.target ?? "") && !(s.optionalTargets ?? []).contains(step.target ?? "") { out.append("\(at): unknown window target '\(step.target ?? "")'") }
            case "openSheet":
                if !sheetTargets.contains(step.target ?? "") && !(s.optionalTargets ?? []).contains(step.target ?? "") { out.append("\(at): unknown sheet target '\(step.target ?? "")'") }
            case "ext": out += UIExtU2.problems(step).map { "\(at): \($0)" }
            case "command": if step.id == nil { out.append("\(at): missing id") }
            case "search": if step.text == nil { out.append("\(at): missing text") }
            case "resizeWindow":
                if step.window == nil || step.contentSize == nil { out.append("\(at): needs window and contentSize") }
            case "press", "toggle": if step.window == nil || step.find == nil { out.append("\(at): needs window and find") }
            case "setText": if step.window == nil || step.find == nil || step.text == nil { out.append("\(at): needs window, find, text") }
            case "closeWindow", "dismissSheet": if step.window == nil { out.append("\(at): missing window") }
            case "reading": out += UIReading.problems(step).map { "\(at): \($0)" }
            case "accounts": out += UIAccounts.problems(step).map { "\(at): \($0)" }
            case "capture": if step.name == nil { out.append("\(at): missing name") }
            case "assert":
                if step.window == nil && (step.rules?.isEmpty == false) { out.append("\(at): rules need a window") }
                for r in step.rules ?? [] where !UIRule.allNames.contains(r) { out.append("\(at): unknown rule '\(r)'") }
            default: break
            }
        }
        return out
    }
}
#endif
