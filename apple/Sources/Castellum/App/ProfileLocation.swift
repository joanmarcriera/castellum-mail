// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import Foundation

/// Where the app keeps its profile (SQLite DB, keybindings, ...).
///
/// S68 /: `open`, `mailto:` and Dock launches go through
/// LaunchServices, which does NOT pass `CASTELLUM_PROFILE_DIR`, so a Debug
/// build launched that way used to open the REAL profile. Rules:
///
///  1. `CASTELLUM_PROFILE_DIR`, when set and non-empty, wins in every build.
///  2. Otherwise a DEBUG build uses `CastellumNativeDev/profile` (sibling of
///     the real `CastellumNative`), so no launch route can reach real mail.
///  3. Otherwise (Release) the real `CastellumNative/profile` — unchanged.
///
/// Mechanism: compile-time `#if DEBUG` (see `isDebugBuild`), not the `.dev`
/// bundle id. A bundle id is runtime data that a copied/re-signed bundle or
/// the XCTest host can carry wrongly; the compiler flag cannot drift from the
/// code that is actually running, and Release builds cannot contain the
/// Debug branch's decision at all.
///
/// Pure: no disk, no environment reads — callers inject all three inputs.
enum ProfileLocation {
    struct Resolved: Equatable {
        let directory: URL
        /// True for anything but the real profile: such a profile gets its own
        /// Keychain namespace (`AccountStore.service(forProfileDirectory:)`).
        let isCustom: Bool
    }

    /// Compile-time Debug flag, the production value for `isDebug`.
    static var isDebugBuild: Bool {
        #if DEBUG
        return true
        #else
        return false
        #endif
    }

    static func resolve(env: [String: String], isDebug: Bool, appSupport: URL) -> Resolved {
        if let override = env["CASTELLUM_PROFILE_DIR"], !override.isEmpty {
            return Resolved(directory: URL(fileURLWithPath: override, isDirectory: true), isCustom: true)
        }
        if isDebug {
            return Resolved(
                directory: appSupport.appendingPathComponent("CastellumNativeDev/profile", isDirectory: true),
                isCustom: true)
        }
        return Resolved(
            directory: appSupport.appendingPathComponent("CastellumNative/profile", isDirectory: true),
            isCustom: false)
    }

    /// Resolve for the running process (real environment, this build's flavour).
    static func current() -> Resolved {
        resolve(
            env: ProcessInfo.processInfo.environment,
            isDebug: isDebugBuild,
            appSupport: FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0])
    }
}
