// Castellum - Share Extension -> main app handoff descriptor (S57).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The Share Extension cannot link the Rust FFI staticlib the way the main
// app does (a sandboxed, short-lived extension process is not the place to
// run IMAP/SMTP/SQLite), so it hands shared content over the simplest
// correct way: write one small JSON descriptor into the App Group's shared
// container, then activate the main app, which reads and deletes it on
// activation. Kept in Core/ (not App/) so BOTH the Castellum app target and
// the CastellumShareExtension target compile this exact same source — one
// format, defined once, with the app-extension gotcha (S57 project.yml) that
// the extension's target membership for this file must be kept in sync by
// hand since there's no shared framework target yet.
//
// No network call anywhere in this file, and nothing here is logged: the
// descriptor persists no more than a normal draft already would once opened.
//
// App Group container protection (macOS 15+). macOS only lets a process
// into ~/Library/Group Containers/<group> silently when its code signature
// AUTHORIZES that group: a Team-ID-prefixed group ("TEAMID0000.x") is
// authorized by the signature's Team ID alone, while an iOS-style "group.x"
// id needs a provisioning profile. Any other access raises the TCC prompt
// "“Castellum” would like to access data from other apps"
// (kTCCServiceSystemPolicyAppData). Before 2026-09-23 this file used
// "group.uk.co.riera.castellum" (no profile anywhere) and read the container
// on every app activation — so every ad-hoc Debug build and XCTest host run
// prompted (tccd log: 16 AUTHREQ_PROMPTING for SystemPolicyAppData, all from
// DerivedData Debug builds), and the next Developer ID release would have
// prompted real users too. `FileManager.containerURL(forSecurity...)` does
// NOT return nil for an unauthorized group on macOS, so the gate has to be
// our own: `authorizedAppGroupId` returns a group only when this process's
// own signature has a Team ID and an entitled group carrying that prefix.
// Otherwise the handoff is off and the container is never touched.

import Foundation
import Security

/// One pending compose request written by the Share Extension.
struct SharePendingCompose: Codable, Equatable {
    var subject: String = ""
    var body: String = ""

    /// The group id suffix. The entitlements files declare
    /// `$(TeamIdentifierPrefix)uk.co.riera.castellum`, which Xcode expands to
    /// "<TEAMID>.uk.co.riera.castellum" for a team-signed build — the
    /// macOS-native form that needs no provisioning profile.
    static let appGroupSuffix = "uk.co.riera.castellum"
    private static let fileName = "pending-compose.json"

    /// Pure decision, split out for tests: the entitled group that the Team ID
    /// authorizes, or nil. Nil for an ad-hoc/unsigned build (no Team ID) and
    /// for any group id the signature cannot authorize without a profile.
    static func authorizedAppGroupId(teamID: String?, entitledGroups: [String]) -> String? {
        guard let teamID, !teamID.isEmpty else { return nil }
        let wanted = "\(teamID).\(appGroupSuffix)"
        return entitledGroups.contains(wanted) ? wanted : nil
    }

    /// This process's Team ID and App Group entitlements, read from its own
    /// code signature (the same facts macOS uses to authorize the group).
    private static func ownSigningFacts() -> (teamID: String?, groups: [String]) {
        var code: SecCode?
        var staticCode: SecStaticCode?
        var info: CFDictionary?
        guard SecCodeCopySelf([], &code) == errSecSuccess, let code,
              SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode,
              SecCodeCopySigningInformation(
                staticCode,
                SecCSFlags(rawValue: kSecCSSigningInformation | kSecCSRequirementInformation),
                &info) == errSecSuccess,
              let dict = info as? [String: Any]
        else { return (nil, []) }
        let team = dict[kSecCodeInfoTeamIdentifier as String] as? String
        let entitlements = dict[kSecCodeInfoEntitlementsDict as String] as? [String: Any]
        let groups = entitlements?["com.apple.security.application-groups"] as? [String] ?? []
        return (team, groups)
    }

    /// Resolved once per process: signatures do not change while running.
    static let appGroupId: String? = {
        let facts = ownSigningFacts()
        return authorizedAppGroupId(teamID: facts.teamID, entitledGroups: facts.groups)
    }()

    private static var containerURL: URL? {
        guard let appGroupId else { return nil }
        return FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroupId)
    }

    private static var fileURL: URL? { containerURL?.appendingPathComponent(fileName) }

    /// Called by the Share Extension after gathering the shared content.
    /// Best-effort: on a build whose signature cannot authorize the group
    /// (ad-hoc Debug, `CODE_SIGNING_ALLOWED=NO` test hosts) `appGroupId` is
    /// nil and this silently does nothing — deliberately, so it never raises
    /// the "access data from other apps" prompt. There is no user-facing
    /// error surface worth adding for what is a signing problem, not a
    /// runtime one.
    func write() {
        guard let fileURL = Self.fileURL, let data = try? JSONEncoder().encode(self) else { return }
        try? data.write(to: fileURL, options: .atomic)
    }

    /// Called by the main app on activation to read and consume any pending
    /// share. Deletes the file so it is never processed twice, even if two
    /// activations race — `removeItem` after a successful read is the only
    /// mutation, so a second caller simply finds nothing.
    static func consumePending() -> SharePendingCompose? {
        guard let fileURL, let data = try? Data(contentsOf: fileURL) else { return nil }
        try? FileManager.default.removeItem(at: fileURL)
        return try? JSONDecoder().decode(SharePendingCompose.self, from: data)
    }

    /// Non-destructive existence check (S57, PR #182 review) — lets a caller
    /// decide whether this launch is a Share Extension handoff (and so needs
    /// `AppDelegate.activateMainWindow()`) WITHOUT consuming the file; only
    /// the actual handoff, `consumePending()`, should do that.
    static var isPending: Bool {
        guard let fileURL else { return false }
        return FileManager.default.fileExists(atPath: fileURL.path)
    }
}


/// Ordered text slots for the Share Extension (#3381). `NSItemProvider.loadItem`
/// completions arrive on arbitrary queues and finish in any order, so each shared item
/// reserves a slot up front in enumeration order and fills it on the main queue only:
/// no concurrent Array mutation, and the body keeps the order the items were shared in.
struct ShareContentSlots {
    private var slots: [String?] = []

    /// Reserve the next slot; returns its index.
    mutating func reserve() -> Int {
        slots.append(nil)
        return slots.count - 1
    }

    /// Fill a reserved slot. An out-of-range index is ignored.
    mutating func fill(_ index: Int, with text: String?) {
        guard slots.indices.contains(index) else { return }
        slots[index] = text
    }

    /// The compose body: filled, non-empty slots in reservation order, blank-line separated.
    var body: String {
        slots.compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: "\n\n")
    }
}
