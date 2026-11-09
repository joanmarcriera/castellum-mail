// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// S68: a Debug build launched by ANY route (`open`, mailto:,
/// Dock — LaunchServices passes no environment) must never open the real
/// profile. Pure-function tests: nothing here touches disk or the real profile.
final class ProfileLocationTests: XCTestCase {
    private let support = URL(fileURLWithPath: "/tmp/castellum-s68-appsupport", isDirectory: true)

    func testEnvOverrideWinsInDebugAndRelease() {
        let env = ["CASTELLUM_PROFILE_DIR": "/tmp/castellum-s68-override"]
        for isDebug in [true, false] {
            let r = ProfileLocation.resolve(env: env, isDebug: isDebug, appSupport: support)
            XCTAssertEqual(r.directory.path, "/tmp/castellum-s68-override")
            XCTAssertTrue(r.isCustom)
        }
    }

    func testEmptyEnvOverrideIsIgnored() {
        let r = ProfileLocation.resolve(env: ["CASTELLUM_PROFILE_DIR": ""], isDebug: false, appSupport: support)
        XCTAssertEqual(r.directory.path, support.appendingPathComponent("CastellumNative/profile").path)
    }

    func testDebugDefaultIsTheDevDirectory() {
        let r = ProfileLocation.resolve(env: [:], isDebug: true, appSupport: support)
        XCTAssertEqual(r.directory.path, support.appendingPathComponent("CastellumNativeDev/profile").path)
        XCTAssertTrue(r.isCustom, "Dev profile needs its own Keychain namespace")
    }

    func testReleaseDefaultIsUnchanged() {
        let r = ProfileLocation.resolve(env: [:], isDebug: false, appSupport: support)
        XCTAssertEqual(r.directory.path, support.appendingPathComponent("CastellumNative/profile").path)
        XCTAssertFalse(r.isCustom, "Release keeps the default Keychain namespace")
    }

    func testDevDirectoryNeverEqualsRealDirectory() {
        let dev = ProfileLocation.resolve(env: [:], isDebug: true, appSupport: support).directory
        let real = ProfileLocation.resolve(env: [:], isDebug: false, appSupport: support).directory
        XCTAssertNotEqual(dev.standardizedFileURL, real.standardizedFileURL)
        XCTAssertFalse(dev.pathComponents.contains("CastellumNative"), "Dev dir must not live inside the real tree")
    }
}
