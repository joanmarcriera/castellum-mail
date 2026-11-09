// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import Foundation

/// Per-process scratch directory for tests, removed when the test host exits.
///
/// Usage: write `TestScratch.root.appendingPathComponent("castellum-x-\(UUID())")`
/// where a test used `FileManager.default.temporaryDirectory`. Tests that
/// already `removeItem` their own directory keep doing so; this is the safety
/// net for the ones that do not (about 120k leaked `castellum-*` entries in
/// `$TMPDIR` were found on 2026-10-05). Only a SIGKILLed test host can leave
/// its `castellum-scratch-<pid>` directory behind.
enum TestScratch {
    /// Created on first use; the exit hook is registered once, at creation.
    static let root: URL = {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-scratch-\(ProcessInfo.processInfo.processIdentifier)", isDirectory: true)
        try? FileManager.default.removeItem(at: dir)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        scratchPathForExit = dir.path
        atexit { removeScratchAtExit() }
        return dir
    }()
}

/// Read by the C exit hook, which cannot capture context.
nonisolated(unsafe) private var scratchPathForExit: String?

private func removeScratchAtExit() {
    if let path = scratchPathForExit { try? FileManager.default.removeItem(atPath: path) }
}
