// Castellum - lightweight GitHub release update check.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// Numeric marketing version used for update decisions. Castellum release tags
/// may carry a leading `v`; two-part versions have an implicit patch of zero.
struct SemanticVersion: Comparable, CustomStringConvertible {
    let major: Int
    let minor: Int
    let patch: Int

    init?(_ value: String) {
        var value = value
        if value.first == "v" || value.first == "V" {
            value.removeFirst()
        }
        let parts = value.split(separator: ".", omittingEmptySubsequences: false)
        guard (2...3).contains(parts.count),
              parts.allSatisfy({ !$0.isEmpty && $0.allSatisfy(\.isNumber) })
        else { return nil }

        let numbers = parts.compactMap { Int($0) }
        guard numbers.count == parts.count else { return nil }
        major = numbers[0]
        minor = numbers[1]
        patch = numbers.count == 3 ? numbers[2] : 0
    }

    static func < (lhs: SemanticVersion, rhs: SemanticVersion) -> Bool {
        (lhs.major, lhs.minor, lhs.patch) < (rhs.major, rhs.minor, rhs.patch)
    }

    var description: String { "\(major).\(minor).\(patch)" }
}

enum UpdateCheckResult: Equatable {
    case upToDate(currentVersion: String)
    case updateAvailable(version: String)
}

enum UpdateCheckError: LocalizedError {
    case invalidResponse
    case invalidVersion

    var errorDescription: String? {
        switch self {
        case .invalidResponse:
            return "The update service returned an unexpected response."
        case .invalidVersion:
            return "The update service returned an invalid version."
        }
    }
}

/// Fetches the public latest-release record and makes pure version/schedule
/// decisions. It sends only GitHub's requested media type: no account, device,
/// installation or other Castellum identifier is attached to the request.
struct UpdateChecker {
    static let latestReleaseURL = URL(
        string: "https://api.github.com/repos/joanmarcriera/castellum-releases/releases/latest"
    )!
    static let downloadURL = URL(
        string: "https://github.com/joanmarcriera/castellum-releases/releases/latest/download/Castellum.dmg"
    )!
    static let automaticInterval: TimeInterval = 24 * 60 * 60

    private struct LatestRelease: Decodable {
        let tagName: String

        enum CodingKeys: String, CodingKey {
            case tagName = "tag_name"
        }
    }

    private let session: URLSession
    private let now: () -> Date

    init(session: URLSession = UpdateChecker.makeSession(),
         now: @escaping () -> Date = Date.init) {
        self.session = session
        self.now = now
    }

    static func makeSession() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpAdditionalHeaders = [:]
        return URLSession(configuration: configuration)
    }

    func makeLatestReleaseRequest() -> URLRequest {
        var request = URLRequest(url: Self.latestReleaseURL)
        request.httpMethod = "GET"
        request.timeoutInterval = 15
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        return request
    }

    func check(currentVersion: String) async throws -> UpdateCheckResult {
        guard let local = SemanticVersion(currentVersion) else {
            throw UpdateCheckError.invalidVersion
        }
        let (data, response) = try await session.data(for: makeLatestReleaseRequest())
        guard let httpResponse = response as? HTTPURLResponse,
              (200..<300).contains(httpResponse.statusCode)
        else { throw UpdateCheckError.invalidResponse }

        let release = try JSONDecoder().decode(LatestRelease.self, from: data)
        guard let remote = SemanticVersion(release.tagName) else {
            throw UpdateCheckError.invalidVersion
        }
        if remote > local {
            return .updateAvailable(version: remote.description)
        }
        return .upToDate(currentVersion: local.description)
    }

    /// `check`, then `record(now)` ONLY if it succeeded. A failed or offline
    /// check must not use up the daily slot (audit #3454), and "Last checked"
    /// must never claim a check that did not happen.
    func checkRecordingSuccess(currentVersion: String,
                               record: (Date) -> Void) async throws -> UpdateCheckResult {
        let result = try await check(currentVersion: currentVersion)
        record(checkTimestamp())
        return result
    }

    /// Whether the after-launch automatic check should run, per the user's
    /// schedule preference. `.never` never runs; `.atLaunch`
    /// always runs, with no 24-hour gate; `.daily` runs at most once per
    /// `automaticInterval`.
    func shouldPerformAutomaticCheck(schedule: UpdateCheckSchedule, lastCheck: Date?) -> Bool {
        switch schedule {
        case .never:
            return false
        case .atLaunch:
            return true
        case .daily:
            guard let lastCheck else { return true }
            return now().timeIntervalSince(lastCheck) >= Self.automaticInterval
        }
    }

    func checkTimestamp() -> Date { now() }

    static func isRemoteVersion(_ remote: String, newerThan local: String) -> Bool {
        guard let remoteVersion = SemanticVersion(remote),
              let localVersion = SemanticVersion(local) else { return false }
        return remoteVersion > localVersion
    }

    static func currentAppVersion(bundle: Bundle = .main) -> String {
        bundle.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.0.0"
    }
}
