// Castellum - process-wide `tracing` init: a rolling, per-profile log file.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Why this exists: before this module there was zero structured logging
// anywhere in the codebase (no os_log/OSLog/Logger in Swift, no log/tracing
// crate in Rust). Two 2026-09-23 incidents were only diagnosable by manually
// `sample`-ing a hung process from the shell. This gives caught errors and
// recovered panics — which today die silently at the call site — somewhere
// retrievable after the fact.
//
// Placement: initialized from `castellum_open_profile`, the one FFI entry
// point every Swift call goes through (see lib.rs's boundary-rules comment).
// That mirrors `install_panic_hook`'s existing once-per-process pattern in
// this same file, and for the same reason: a subscriber is a global resource
// (`tracing::subscriber::set_global_default` can only succeed once), so it is
// installed exactly once, keyed off the FIRST profile a process opens.
//
// Per-profile isolation: the log directory is `<profile>/logs`, i.e. the
// SAME directory that already holds `castellum.db` — never a shared/global
// path. This matters for exactly the reason `AccountStore.service(
// forProfileDirectory:)` and `TestHost.profileDirectory` give for Keychain
// isolation: a throwaway XCTest profile must never read from or write into
// the real profile's state, or a test run leaks into (or reads from) a
// developer's live data. `TestHost.profileDirectory` opens ONE fresh temp
// profile per test-host PROCESS (not per test method), so "first profile
// opened in this process" and "the profile whose logs these are" are the
// same directory in every real case — app launch, GUI smoke
// (CASTELLUM_PROFILE_DIR), and `xcodebuild test`. A process that
// legitimately opens more than one profile (only seen in ad hoc Rust unit
// tests that call `castellum_core::open_profile` directly, never through this
// FFI) keeps logging to whichever profile got here first; that is a known,
// accepted limitation of a process-global subscriber, not a leak.
//
// Rotation/retention: daily rotation, capped at 14 files (~2 weeks) so an
// unattended machine can't fill its disk from logging — see the
// castellum-batch-runs-fill-the-disk lesson (this machine has had real
// disk-full incidents from unbounded growth before).
//
// Overhead: INFO+ only by default (no per-call/per-loop tracing — only
// meaningful events: sync/replay/send failures, panic recoveries, mutex
// poisoning recoveries, provider connect failures). `RUST_LOG` overrides for
// local debugging, e.g. `RUST_LOG=debug` or `RUST_LOG=castellum_core=debug`.
//
// Discipline (mirrors panic_guard.rs's doc comment and CLAUDE.md gotcha 5):
// no secrets, no message content, no raw server bytes in any log line ever
// written through this subscriber. Every call site in this codebase that
// logs must pass sanitized data only (an error's redacted `kind`/summary via
// `castellum_core::redact`, an operation name, an account id — never a raw
// error's `Display`/`Debug` text, which can embed arbitrary server-controlled
// bytes).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();

/// The subdirectory of a profile that holds its log files.
const LOG_SUBDIR: &str = "logs";

/// The log directory for a given profile path — `<profile>/logs`. Exposed
/// (not just used internally) so `castellum_log_directory` can hand it to
/// Swift without duplicating the join logic.
pub fn log_directory(profile_dir: &Path) -> PathBuf {
    profile_dir.join(LOG_SUBDIR)
}

/// Initialise the process-wide `tracing` subscriber, writing INFO+ (or
/// `RUST_LOG`, if set) to a daily-rotating file under `<profile_dir>/logs`.
///
/// Idempotent: only the FIRST call in a process actually installs anything;
/// later calls (a second profile opened in the same process, or a caller
/// racing `castellum_open_profile` on two threads) are no-ops. Never panics
/// and never fails loudly — a missing log destination must not stop the app
/// from working, so every error path here just leaves logging off.
pub fn init_logging(profile_dir: &Path) {
    INIT.call_once(|| {
        let logs_dir = log_directory(profile_dir);
        if std::fs::create_dir_all(&logs_dir).is_err() {
            return;
        }
        let appender = match Builder::new()
            .rotation(Rotation::DAILY)
            .filename_prefix("castellum")
            .filename_suffix("log")
            // Bound total disk use: ~2 weeks of daily files, then oldest is
            // dropped. See the module doc comment.
            .max_log_files(14)
            .build(&logs_dir)
        {
            Ok(appender) => appender,
            Err(_) => return,
        };

        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        // `RollingFileAppender` isn't `Clone`/`Sync` on its own; wrapping in a
        // `Mutex` is the standard way to hand it to `fmt()` as a `MakeWriter`
        // (tracing_subscriber has a blanket `MakeWriter` impl for `Mutex<W:
        // Write>`). Writes are synchronous and infrequent (INFO+ only, no
        // hot-path logging), so no non-blocking/background-worker writer is
        // needed here.
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Mutex::new(appender))
            .with_ansi(false)
            .with_env_filter(filter)
            .finish();

        // Best-effort: if some other subscriber already won (unexpected
        // outside tests, since this is Once-guarded), logging is diagnostic
        // only and never load-bearing — carry on without it.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_directory_is_a_logs_subdir_of_the_profile() {
        let dir = log_directory(Path::new("/tmp/some-profile"));
        assert_eq!(dir, Path::new("/tmp/some-profile/logs"));
    }
}
