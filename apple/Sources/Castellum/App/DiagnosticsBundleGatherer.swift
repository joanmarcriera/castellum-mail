// Castellum - crash-report, app-log and Rust-log gathering for the
// diagnostics bundle.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Reads three LOCAL, non-database sources: this app's own recent crash
// reports (~/Library/Logs/DiagnosticReports), a short excerpt of the unified
// system log for this process, and (as of PR #148/#150's companion diagnostics
// work) a tail excerpt of the Rust core's own structured `tracing` log file
// (<profile>/logs, via CastellumCore.logDirectory() / castellum_log_directory).
// All three are LOCAL reads only — nothing here opens a network connection.
// Every returned string still passes through CastellumCore.redactDiagnosticText
// before DiagnosticsViewModel writes it to disk (see DiagnosticsView.swift).

import Foundation

/// One crash/spin/hang report already filtered to this app and capped in size.
struct CrashReportFile: Equatable {
    let filename: String
    let contents: String
}

enum DiagnosticsBundleGatherer {
    // MARK: - Crash reports

    /// Filename prefixes this app's own crash/spin/hang reports use.
    /// `~/Library/Logs/DiagnosticReports` is shared by every app on the Mac;
    /// matching by filename prefix — with a `-`/`_` boundary right after it —
    /// is what keeps another app's report (or a same-prefixed but different
    /// process, e.g. a hypothetical "CastellumHelper") out of the bundle.
    static let crashReportPrefixes = ["Castellum-", "Castellum_"]

    static let maxCrashReports = 5
    static let maxCrashReportBytes = 200_000
    static let maxCrashReportAgeDays = 30

    /// The most recent Castellum-only crash/spin/hang reports, newest first,
    /// each truncated to `maxCrashReportBytes`. Returns an empty array (never
    /// throws) when the directory is missing, unreadable, or empty — a fresh
    /// install has no reports yet, which is not an error.
    static func recentCrashReports(
        in directory: URL = defaultDiagnosticReportsDirectory,
        now: Date = Date(),
        fileManager: FileManager = .default
    ) -> [CrashReportFile] {
        guard let entries = try? fileManager.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: [.contentModificationDateKey],
            options: [.skipsHiddenFiles]
        ) else { return [] }

        let cutoff = now.addingTimeInterval(-Double(maxCrashReportAgeDays) * 86_400)
        let matches = entries.filter { url in
            crashReportPrefixes.contains { url.lastPathComponent.hasPrefix($0) }
        }
        let dated: [(URL, Date)] = matches.compactMap { url in
            guard let values = try? url.resourceValues(forKeys: [.contentModificationDateKey]),
                  let modified = values.contentModificationDate,
                  modified >= cutoff
            else { return nil }
            return (url, modified)
        }
        let newest = dated.sorted { $0.1 > $1.1 }.prefix(maxCrashReports)

        return newest.compactMap { (url, _) in
            guard let data = try? Data(contentsOf: url) else { return nil }
            let truncated = data.prefix(maxCrashReportBytes)
            var text = String(decoding: truncated, as: UTF8.self)
            if data.count > maxCrashReportBytes {
                text += "\n\n[…truncated at \(maxCrashReportBytes) bytes…]"
            }
            return CrashReportFile(filename: url.lastPathComponent, contents: text)
        }
    }

    static var defaultDiagnosticReportsDirectory: URL {
        FileManager.default.urls(for: .libraryDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Logs/DiagnosticReports", isDirectory: true)
    }

    // MARK: - App log

    static let appLogLookback = "1h"
    static let maxAppLogBytes = 300_000
    static let logShowTimeout: TimeInterval = 10

    /// The arguments passed to `/usr/bin/log show`. Extracted as a pure
    /// function so the predicate — which is what keeps another process's log
    /// lines out of the bundle — is directly testable without spawning a
    /// process.
    static func logShowArguments(processName: String, lookback: String) -> [String] {
        ["show", "--predicate", "process == \"\(processName)\"", "--style", "compact", "--last", lookback]
    }

    /// A short excerpt of the unified system log for this process (launch,
    /// termination, uncaught exceptions — Castellum calls no logging API with
    /// mail content or credentials anywhere in this codebase, so there is
    /// nothing else in here). Shells out to `/usr/bin/log show`, a LOCAL read
    /// of the on-disk log store — no socket is opened. Returns an empty
    /// string, never throws, if `log` is slow, missing, or denied, so a
    /// diagnostics gather can never hang or fail because of this step alone.
    /// Call off the main thread — this may block for the duration of `log show`.
    static func recentAppLog(
        processName: String = "Castellum",
        lookback: String = appLogLookback,
        executableURL: URL = URL(fileURLWithPath: "/usr/bin/log"),
        timeout: TimeInterval = logShowTimeout
    ) -> String {
        let process = Process()
        process.executableURL = executableURL
        process.arguments = logShowArguments(processName: processName, lookback: lookback)
        let stdout = Pipe()
        process.standardOutput = stdout
        process.standardError = Pipe() // discarded

        // Drain stdout concurrently as it arrives rather than after the
        // process exits: `log show --last 1h` can emit well over the pipe's
        // ~64KB kernel buffer, and a child blocked on a full pipe never
        // exits — reading only after `isRunning` goes false silently
        // returned "" once the excerpt was large, despite the 300KB cap
        // implying larger output is supported.
        let outputLock = NSLock()
        var collected = Data()
        stdout.fileHandleForReading.readabilityHandler = { handle in
            let chunk = handle.availableData
            guard !chunk.isEmpty else { return }
            outputLock.lock()
            collected.append(chunk)
            outputLock.unlock()
        }

        do {
            try process.run()
        } catch {
            stdout.fileHandleForReading.readabilityHandler = nil
            return ""
        }

        let deadline = Date().addingTimeInterval(timeout)
        while process.isRunning && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        let timedOut = process.isRunning
        if timedOut {
            process.terminate()
        }
        // One final synchronous drain for any chunk delivered right as the
        // process exited, before tearing the handler down.
        let tail = stdout.fileHandleForReading.availableData
        stdout.fileHandleForReading.readabilityHandler = nil

        if timedOut {
            return ""
        }

        outputLock.lock()
        collected.append(tail)
        let data = collected
        outputLock.unlock()

        let truncated = data.prefix(maxAppLogBytes)
        var text = String(decoding: truncated, as: UTF8.self)
        if data.count > maxAppLogBytes {
            text += "\n\n[…truncated at \(maxAppLogBytes) bytes…]"
        }
        return text
    }

    // MARK: - Rust core log excerpt

    static let maxRustLogBytes = 300_000

    /// A tail excerpt of the Rust core's own structured `tracing` log — the
    /// most-recently-modified file inside `directory` (the daily-rotating
    /// file it is actively writing to), capped at `maxRustLogBytes`. Returns
    /// `""`, never throws, when `directory` is `nil`, missing, empty or
    /// unreadable: a fresh profile (or one whose Rust core has never logged
    /// at INFO+ yet — `castellum_log_directory` documents that as normal, not
    /// an error) has nothing here, and a gather-and-save must never fail or
    /// hang because of this optional extra.
    ///
    /// Head vs. tail: `recentAppLog` above keeps `data.prefix(maxAppLogBytes)`
    /// — the OLDEST bytes — but that is safe there only because `log show
    /// --last 1h` already scopes its OWN output to a bounded recent window,
    /// so "oldest byte in an already-recent window" is still recent. The Rust
    /// log file has no such external bound: it is a plain chronological,
    /// append-only file that can grow far larger than the cap over a day's
    /// use. Keeping the head there would silently show only whatever
    /// happened first that day and cut off exactly the messages nearest "what
    /// was the app doing right before this went wrong" — the whole point of
    /// including it. `data.suffix(maxRustLogBytes)` (the newest bytes) is
    /// therefore the correct truncation direction here, the opposite of
    /// `recentAppLog`'s.
    static func recentRustLog(
        in directory: URL?,
        fileManager: FileManager = .default
    ) -> String {
        guard let directory,
              let entries = try? fileManager.contentsOfDirectory(
                  at: directory,
                  includingPropertiesForKeys: [.contentModificationDateKey],
                  options: [.skipsHiddenFiles]
              ),
              !entries.isEmpty
        else { return "" }

        func modified(_ url: URL) -> Date {
            (try? url.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate
                ?? .distantPast
        }
        guard let newest = entries.max(by: { modified($0) < modified($1) }),
              let data = try? Data(contentsOf: newest)
        else { return "" }

        if data.count <= maxRustLogBytes {
            return String(decoding: data, as: UTF8.self)
        }
        let tail = data.suffix(maxRustLogBytes)
        return "[…truncated, showing only the last \(maxRustLogBytes) bytes…]\n\n"
            + String(decoding: tail, as: UTF8.self)
    }
}
