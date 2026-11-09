// Castellum - shared guarded IMAP connect factory.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `sync.rs` and `replay.rs` each need a live provider wrapped in
// `PanicGuardedProvider` before doing anything else — connect runs LOGIN and
// reads CAPABILITY, exactly the `imap-proto` 0.10.2 parser sites that can
// panic on unparsable server input instead of erroring (CLAUDE.md gotcha 1).
// Wrapping only the returned provider leaves that exchange OUTSIDE the guard,
// so this module gives every call site one place to get it right: connect
// AND wrap in a single call, never connect-then-wrap.
//
// `connect_guarded_with` is the one place `PanicGuardedProvider::connect_with`
// is called anywhere in the crate: both `connect_guarded_imap` (the real
// production connector) and `sync.rs`'s test-injection seam
// (`sync_imap_json_with`, which takes the connector as a parameter so a
// panicking fake can be injected without a hostile server) route through it,
// so there is exactly one guard-construction site to get right.

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, MailProvider, PanicGuardedProvider, ProviderError};

/// Connect and guard a provider from an injected connector. The single place
/// `PanicGuardedProvider::connect_with` is called: a panic inside LOGIN/
/// CAPABILITY is caught and surfaced as a `ProviderError` instead of
/// unwinding past the caller's diagnostic recording.
///
/// Error mapping is unchanged from the call sites this replaces: whatever
/// `connect` or the panic guard returns comes straight back.
pub(crate) fn connect_guarded_with<P: MailProvider>(
    connect: impl FnOnce(&ImapConfig) -> Result<P, ProviderError>,
    cfg: &ImapConfig,
) -> Result<PanicGuardedProvider<P>, ProviderError> {
    PanicGuardedProvider::connect_with(|| connect(cfg))
}

/// Connect a guarded IMAP provider. The production entry point for every
/// real (non-test-injected) IMAP connect in sync and replay: routes
/// `ImapProvider::connect` through [`connect_guarded_with`].
pub(crate) fn connect_guarded_imap(
    cfg: &ImapConfig,
) -> Result<PanicGuardedProvider<ImapProvider>, ProviderError> {
    connect_guarded_with(ImapProvider::connect, cfg)
}

#[cfg(test)]
mod tests {
    /// A production (non-#[cfg(test)]) raw `ImapProvider::connect(` call in
    /// `sync.rs` or `replay.rs` means a new call site bypassed
    /// `connect_guarded_imap` and reintroduced the connect-then-wrap gap this
    /// module exists to close. Both files put their whole `mod tests` after a
    /// single `#[cfg(test)]`, so everything before that marker is production
    /// code.
    fn assert_no_raw_connect_outside_tests(file: &str, source: &str) {
        let production = match source.find("#[cfg(test)]") {
            Some(idx) => &source[..idx],
            None => source,
        };
        assert!(
            !production.contains("ImapProvider::connect("),
            "{file}: found a raw `ImapProvider::connect(` outside #[cfg(test)] — \
             route it through `imap_connect::connect_guarded_imap` instead"
        );
        // A hand-written `PanicGuardedProvider::connect_with(` (or `::new(`) is
        // a second guard-construction site duplicating `connect_guarded_with`
        // — exactly the gap that let sync.rs and imap_connect.rs both guard a
        // connect independently before this factory existed. Production code
        // must always go through `imap_connect::connect_guarded_imap` or
        // `imap_connect::connect_guarded_with`, never call the guard directly.
        assert!(
            !production.contains("PanicGuardedProvider::connect_with(")
                && !production.contains("PanicGuardedProvider::new("),
            "{file}: found a hand-written `PanicGuardedProvider` guard \
             construction outside #[cfg(test)] — route it through \
             `imap_connect::connect_guarded_imap`/`connect_guarded_with` instead"
        );
    }

    #[test]
    fn sync_rs_has_no_raw_connect_outside_tests() {
        assert_no_raw_connect_outside_tests("sync.rs", include_str!("sync.rs"));
    }

    #[test]
    fn replay_rs_has_no_raw_connect_outside_tests() {
        assert_no_raw_connect_outside_tests("replay.rs", include_str!("replay.rs"));
    }
}
