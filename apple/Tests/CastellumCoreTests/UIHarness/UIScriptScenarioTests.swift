// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1: every committed scenario must decode and validate, so a typo
// in an op, target, rule or wait condition fails CI instead of the first manual run.
// Scenario files: scripts/ui-scenarios/*.json (+ negative/*.json).
import XCTest
@testable import Castellum

@MainActor
final class UIScriptScenarioTests: XCTestCase {
    private var scenarioDir: URL {
        // <repo>/apple/Tests/CastellumCoreTests/UIHarness/<this file>
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("scripts/ui-scenarios", isDirectory: true)
    }

    private func scenarioFiles() throws -> [URL] {
        let fm = FileManager.default
        var out: [URL] = []
        for sub in ["", "negative"] {
            let dir = scenarioDir.appendingPathComponent(sub)
            let items = (try? fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)) ?? []
            out += items.filter { $0.pathExtension == "json" }
        }
        return out
    }

    func testEveryScenarioDecodesAndValidates() throws {
        let files = try scenarioFiles()
        try XCTSkipIf(files.isEmpty, "scenario directory not found from #filePath (test bundle relocated)")
        for f in files {
            let scenario: UIScenario
            do { scenario = try JSONDecoder().decode(UIScenario.self, from: Data(contentsOf: f)) }
            catch { XCTFail("\(f.lastPathComponent) does not decode: \(error)"); continue }
            XCTAssertEqual(UIScenarioValidator.problems(in: scenario), [], f.lastPathComponent)
            XCTAssertFalse(scenario.covers?.isEmpty ?? true, "\(f.lastPathComponent) should say what it covers")
        }
    }

    func testValidatorRejectsBadScenarios() {
        func scenario(_ steps: [UIStep]) -> UIScenario {
            UIScenario(schema: 1, name: "t", steps: steps)
        }
        XCTAssertFalse(UIScenarioValidator.problems(in: scenario([UIStep(op: "explode")])).isEmpty)
        XCTAssertFalse(UIScenarioValidator.problems(in: scenario([UIStep(op: "openSheet", target: "nope")])).isEmpty)
        XCTAssertFalse(UIScenarioValidator.problems(in: scenario([UIStep(op: "wait", until: "whenever")])).isEmpty)
        XCTAssertFalse(UIScenarioValidator.problems(in: scenario([UIStep(op: "assert", window: "main", rules: ["nope"])])).isEmpty)
        XCTAssertEqual(UIScenarioValidator.problems(in: scenario([UIStep(op: "wait", until: "idle")])), [])
    }

    func testUnsafeEnvironmentIsRefused() {
        // No CASTELLUM_PROFILE_DIR under XCTest, so the runner must refuse rather than run.
        let model = UISnapshotHarness.makeModel()
        XCTAssertNotNil(UIScriptRunner.unsafeReason(model: model))
    }

    /// Every `UIScriptSafety.reason` branch, one call each, plus the all-clear.
    func testUnsafeReasonCoversEveryBranch() {
        let tmp = NSTemporaryDirectory()
        let goodProfile = URL(fileURLWithPath: tmp).appendingPathComponent("uit-profile").path
        func reason(bundle: String? = "uk.co.riera.castellum.uitest.abc", env: String? = nil, profile: String? = nil,
                    keychain: Bool = true, service: String? = "ns", defaultService: String = "real") -> String? {
            UIScriptSafety.reason(bundleID: bundle, envProfile: env ?? goodProfile, profile: profile ?? goodProfile,
                                  tmp: tmp, inMemoryKeychain: keychain, keychainService: service, defaultService: defaultService)
        }
        XCTAssertNil(reason(), "all guards satisfied")
        XCTAssertNil(reason(bundle: "uk.co.riera.castellum.uitest"), "the bare uitest id is accepted")
        XCTAssertNil(reason(service: nil), "early phase skips the namespace check")
        // bundle id
        XCTAssertTrue(reason(bundle: "uk.co.riera.castellum.dev")?.contains("bundle id") == true)
        XCTAssertTrue(reason(bundle: "uk.co.riera.castellum.uitestX")?.contains("bundle id") == true, "prefix must end at a dot")
        XCTAssertTrue(reason(bundle: nil)?.contains("bundle id") == true)
        // env profile
        XCTAssertTrue(UIScriptSafety.reason(bundleID: "uk.co.riera.castellum.uitest", envProfile: nil, profile: goodProfile, tmp: tmp,
            inMemoryKeychain: true, keychainService: "ns", defaultService: "real")?.contains("not set") == true)
        XCTAssertTrue(UIScriptSafety.reason(bundleID: "uk.co.riera.castellum.uitest", envProfile: "", profile: goodProfile, tmp: tmp,
            inMemoryKeychain: true, keychainService: "ns", defaultService: "real")?.contains("not set") == true)
        // profile mismatch
        XCTAssertTrue(reason(profile: goodProfile + "-other")?.contains("is not CASTELLUM_PROFILE_DIR") == true)
        // real-profile paths
        let real = NSHomeDirectory() + "/Library/Application Support/CastellumNativeDev/profile"
        XCTAssertTrue(reason(env: real, profile: real)?.contains("real profile") == true)
        let dbg = NSHomeDirectory() + "/.castellum-debug-profile"
        XCTAssertTrue(reason(env: dbg, profile: dbg)?.contains("real profile") == true)
        // outside temp and build/
        XCTAssertTrue(reason(env: "/var/elsewhere/p", profile: "/var/elsewhere/p")?.contains("neither under the temp dir") == true)
        XCTAssertNil(reason(env: "/x/build/p", profile: "/x/build/p"), "a build/ dir is allowed")
        // keychain
        XCTAssertTrue(reason(keychain: false)?.contains("not in-memory") == true)
        XCTAssertTrue(reason(service: "real")?.contains("real namespace") == true)
    }

    func testSkipOnlyForDeclaredOptionalTargetsMissingFromRegistry() {
        func sc(_ steps: [UIStep], optional: [String]? = nil) -> UIScenario {
            var s = UIScenario(schema: 1, name: "t", steps: steps); s.optionalTargets = optional; return s
        }
        // A typo is invalid, never a skip.
        let typo = sc([UIStep(op: "openSheet", target: "addAcount")])
        XCTAssertFalse(UIScenarioValidator.problems(in: typo).isEmpty)
        XCTAssertEqual(UIScenarioValidator.missingOptionalTargets(in: typo), [])
        // Declared optional and absent: valid, but reported as missing (runner exits 3).
        let opt = sc([UIStep(op: "openSheet", target: "futureSheet")], optional: ["futureSheet"])
        XCTAssertEqual(UIScenarioValidator.problems(in: opt), [])
        XCTAssertEqual(UIScenarioValidator.missingOptionalTargets(in: opt), ["futureSheet"])
        // Declared optional but present: not missing.
        let present = sc([UIStep(op: "openSheet", target: "addAccount")], optional: ["addAccount"])
        XCTAssertEqual(UIScenarioValidator.missingOptionalTargets(in: present), [])
    }

    func testSheetLabelHasNoFallbackToAnyAttachedSheet() {
        XCTAssertNil(UIWindowLabels().resolve("sheet:nonexistent"))
    }

    func testTestHostUIScriptSwitchesArePureDefaults() {
        // Under XCTest there is no CASTELLUM_UI_SCRIPT, so the harness stays off.
        XCTAssertFalse(TestHost.isUIScript)
        XCTAssertTrue(TestHost.suppressesLaunchSideEffects)   // XCTest still suppresses
        XCTAssertTrue(TestHost.usesInMemoryKeychain)
    }
}
