// Castellum - manual proof that the tracing pipeline actually writes a file.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Not a test (the subscriber is process-global and `cargo test` runs many
// processes/threads sharing one binary, so asserting on global logging state
// there is unreliable). This is a standalone binary, same spirit as
// `castellum-core`'s `examples/sync_bench.rs`: run it by hand (or from CI) to
// see the real pipeline work end to end — profile open -> logging init ->
// a caught provider panic -> a line on disk.
//
// Run: cargo run -p castellum-ffi --example logging_demo
//
// Expected output: the script prints the log directory, then the ERROR line
// `panic_guard.rs` wrote for the injected panic in `Boom::select`, read back
// from the actual rolling log file it just wrote.

use castellum_providers::{
    MailProvider, MailboxState, PanicGuardedProvider, ProviderError, RemoteMailbox, RemoteMessage,
};

/// Minimal `MailProvider` that panics on `select`, standing in for the real
/// `imap`/`imap-proto` panics `PanicGuardedProvider` exists to catch (see
/// panic_guard.rs's doc comment). Mirrors the `Exploding` fixture in that
/// module's own tests.
struct Boom;

impl MailProvider for Boom {
    fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
        Ok(vec![])
    }

    fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
        panic!("demo panic standing in for an imap-proto parser panic");
    }

    fn fetch_new(&mut self, _after_uid: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
        Ok(vec![])
    }

    fn logout(&mut self) -> Result<(), ProviderError> {
        Ok(())
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!(
        "castellum-logging-demo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    // Exercises the exact path Swift does through `castellum_open_profile`:
    // create the profile dir, open the DB, and — the point of this demo —
    // initialize the rolling-file subscriber for it.
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    castellum_ffi::logging::init_logging(profile.path());

    println!("profile dir:  {}", dir.display());
    let log_dir = castellum_ffi::logging::log_directory(profile.path());
    println!("log dir:      {}", log_dir.display());

    tracing::info!("logging_demo: subscriber initialized, about to trigger a guarded panic");

    // Trigger the ERROR-level breadcrumb added to PanicGuardedProvider::guard.
    let mut provider = PanicGuardedProvider::new(Boom);
    let err = provider
        .select("INBOX")
        .expect_err("Boom always panics on select");
    println!("provider returned (not panicked): {err}");
    assert!(
        provider.is_abandoned(),
        "session must be abandoned after the panic"
    );

    // The fmt subscriber writes synchronously (no non-blocking buffering, see
    // logging.rs), so the line is on disk before we read it back.
    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .expect("log directory should exist")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !entries.is_empty(),
        "expected at least one log file in {log_dir:?}"
    );

    let mut found_error_line = false;
    for entry in &entries {
        let contents = std::fs::read_to_string(entry.path()).unwrap_or_default();
        println!("\n--- {} ---\n{contents}", entry.path().display());
        if contents.contains("provider panic caught; session abandoned")
            && contents.contains("ERROR")
        {
            found_error_line = true;
        }
    }
    assert!(
        found_error_line,
        "expected the panic_guard ERROR line in one of the log files under {log_dir:?}"
    );

    println!("\nOK: found the panic_guard ERROR line in the rolling log file.");
    let _ = std::fs::remove_dir_all(&dir);
}
