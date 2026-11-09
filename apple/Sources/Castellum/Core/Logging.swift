// Castellum - unified logging (os_log / Logger) categories.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Before this file there was zero structured logging anywhere in this
// codebase (Swift or Rust) — no os_log/OSLog/Logger in any Swift file. Two
// 2026-09-23 incidents were only diagnosable by manually `sample`-ing a hung
// process from the shell. This is the Swift half of an observability slice
// (see the Rust core's `castellum-ffi::logging` for the file-backed half);
// unified logging is free-standing — it integrates with Console.app/`log
// show` on its own, no file management needed here.
//
// Subsystem: `Bundle.main.bundleIdentifier`, not a hardcoded literal.
// Since PR #141, Debug builds carry the `.dev` suffix (`uk.co.riera.castellum.dev`,
// "Castellum Dev") while Release stays `uk.co.riera.castellum` — both driven by
// `CASTELLUM_BUNDLE_ID_SUFFIX` in `apple/configs/Direct.xcconfig` /
// `DirectRelease.xcconfig` and appended in `apple/project.yml`'s
// `PRODUCT_BUNDLE_IDENTIFIER`. Reading `Bundle.main.bundleIdentifier` at
// runtime already picks up whichever one the build was signed with, so no
// special-casing is needed here either way. The fallback literal only covers
// a host process with no bundle at all, which has never been observed in
// practice.
//
// Categories mirror the areas that actually generate incidents: `sync`
// (IMAP/SMTP), `accounts` (add/reconnect/credentials), `lifecycle` (launch/
// profile/bootstrap breadcrumbs), `ui` (an FFI/core error surfaced only in
// an alert today).
//
// Discipline (same as the Rust core's `panic_guard.rs`/`redact.rs` and
// CLAUDE.md gotcha 5's capability-sanitization spirit): NEVER log secrets,
// message content, or credentials. Any interpolated value that could carry
// user data (account id, email, raw error text) MUST be marked
// `privacy: .private` explicitly — Swift's `Logger` already defaults
// non-literal interpolations to private, but this file marks it explicitly
// at every call site so the intent is visible in review, not just inherited
// from a default. Only structured, allowlisted values (an enum's
// `.rawValue`, a redacted `ErrorKind` string, a counter) are ever `.public`.
//
// Under XCTest (`TestHost.isActive`): unified logging is safe to call from a
// test host (no crash risk, near-zero cost), so nothing here suppresses it
// outright. Breadcrumbs are logged at `.info`, which macOS does NOT persist
// to disk by default (only `.notice`/default and above are always
// persisted) — so a test run's routine logging doesn't accumulate in a
// developer's Console.app the way `print()` would. Only `.error`/`.fault`
// (real failures, worth surfacing even from a test run) are always
// persisted, which is the point.

import Foundation
import os

enum Log {
    private static let subsystem = Bundle.main.bundleIdentifier ?? "uk.co.riera.castellum"

    /// IMAP/SMTP sync, replay, and send.
    static let sync = Logger(subsystem: subsystem, category: "sync")
    /// Account add/reconnect/credentials.
    static let accounts = Logger(subsystem: subsystem, category: "accounts")
    /// App/profile lifecycle: launch, profile opened, bootstrap.
    static let lifecycle = Logger(subsystem: subsystem, category: "lifecycle")
    /// Errors surfaced through the UI (alerts, inline banners) that would
    /// otherwise be visible only while the user is looking at the screen.
    static let ui = Logger(subsystem: subsystem, category: "ui")
    /// Performance instrumentation — currently just the DEBUG-only
    /// `MainThreadHitchProbe`. Not written to by anything
    /// that ships to a real user's Release build.
    static let perf = Logger(subsystem: subsystem, category: "perf")
}
