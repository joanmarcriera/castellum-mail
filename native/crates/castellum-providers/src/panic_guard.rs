// Castellum - turn a provider-library panic into an ordinary provider error.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The IMAP stack we depend on panics on server input it cannot handle rather
// than returning an error. Known, reachable examples against real servers:
//
//   * `imap` 2.4.1 `Connection::read_response_onto` (client.rs:1369)
//     `assert_eq!`s the response tag — ANY desynchronised reply aborts.
//   * `imap` 2.4.1 `parse_mailbox` (parse.rs:210) hits `unreachable!()` when a
//     SELECT reply carries an untagged non-OK line (`* NO …`), which some
//     servers do send.
//   * `imap-proto` 0.10.2 `msg_att_internal_date` (rfc3501.rs:379) unwraps a
//     `NIL` INTERNALDATE.
//
// A panic there unwinds the whole sync, and (before the mutex-recovery work)
// bricked the profile. We cannot fix the dependencies from here, but we can
// stop one hostile or merely unusual response from being fatal: this decorator
// wraps any `MailProvider` so a panic becomes a `ProviderError`.
//
// A panic mid-protocol leaves the session state UNKNOWN — an unread tagged line
// may still be on the socket, so the next command would read the wrong reply
// (that is exactly the failure chain the Gmail pilot crash went through). The
// guard therefore abandons the session permanently: every later call fails fast
// and the caller must reconnect. Fail-closed beats compounding a desync.
//
// The error text deliberately names only the OPERATION, never the panic
// payload: panic messages from these parsers embed raw server bytes, and
// provider-controlled text must not flow into an error string that gets
// surfaced or logged. The FFI's panic hook still records the real `file:line`
// for diagnostics.

use crate::{
    AppendedUid, DescriptorChunkResult, Guarded, IdleWake, KeywordStoreOutcome, MailProvider,
    MailboxState, MessageFlag, MessageIdMatches, ProviderError, QresyncSync, RemoteMessage,
    RemoteMessageDescriptor, SessionAbandonReason, UidFlags,
};
use crate::{MovedTo, RemoteMailbox};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Wraps a `MailProvider` so a panic in the underlying protocol library is
/// reported as a `ProviderError` and the session is abandoned.
pub struct PanicGuardedProvider<P: MailProvider> {
    inner: P,
    /// `Some(operation)` once a call panicked; the session is unusable after.
    abandoned_by: Option<&'static str>,
}

impl<P: MailProvider> PanicGuardedProvider<P> {
    pub fn new(inner: P) -> Self {
        PanicGuardedProvider {
            inner,
            abandoned_by: None,
        }
    }

    /// True once a panic has abandoned this session.
    pub fn is_abandoned(&self) -> bool {
        self.abandoned_by.is_some()
    }

    /// Connect under the same guard, then wrap the result.
    ///
    /// A provider's `connect` is NOT a cheap constructor: it opens the socket,
    /// runs LOGIN and reads CAPABILITY — precisely the `read_response_onto`
    /// tag-assert and capability-parser sites this module exists for. Calling
    /// `Provider::connect(...)` bare and only then wrapping leaves that whole
    /// exchange OUTSIDE the guard, so a panic there unwinds past the caller's
    /// error handling and the account never records the failure (its sync
    /// attempt is left with no error and `consecutive_failures` never moves).
    /// Use this instead of `connect(...).map(PanicGuardedProvider::new)`.
    ///
    /// Like `guard`, the error names only the operation — never the panic
    /// payload, which embeds raw server bytes.
    pub fn connect_with(
        connect: impl FnOnce() -> Result<P, ProviderError>,
    ) -> Result<Self, ProviderError> {
        match catch_unwind(AssertUnwindSafe(connect)) {
            Ok(result) => result.map(PanicGuardedProvider::new),
            Err(_) => {
                // Operation name only — NEVER the panic payload, which embeds
                // raw server bytes (see this module's doc comment and
                // `the_panic_payload_never_leaks_server_bytes_into_the_error`
                // below, which enforces the same discipline on the error text).
                tracing::error!(
                    operation = "connect",
                    "provider panic caught; session abandoned before it started"
                );
                Err(ProviderError::Protocol(
                    "imap connect crashed the protocol parser on this server's response; \
                     the connection was abandoned"
                        .to_string(),
                ))
            }
        }
    }

    fn guard<T>(
        &mut self,
        operation: &'static str,
        call: impl FnOnce(&mut P) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        if let Some(previous) = self.abandoned_by {
            return Err(ProviderError::Protocol(format!(
                "imap session abandoned after {previous} crashed the protocol parser; \
                 reconnect required"
            )));
        }
        let inner = &mut self.inner;
        match catch_unwind(AssertUnwindSafe(|| call(inner))) {
            Ok(result) => result,
            Err(_) => {
                self.abandoned_by = Some(operation);
                // Operation name only — never the panic payload (see
                // `connect_with` above and this module's doc comment).
                tracing::error!(operation, "provider panic caught; session abandoned");
                Err(ProviderError::Protocol(format!(
                    "imap {operation} crashed the protocol parser on this server's \
                     response; the session is desynchronised and was abandoned"
                )))
            }
        }
    }
}

impl<P: MailProvider> MailProvider for PanicGuardedProvider<P> {
    fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
        self.guard("list_mailboxes", |p| p.list_mailboxes())
    }

    fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
        self.guard("select", |p| p.select(path))
    }

    fn fetch_new(&mut self, after_uid: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
        self.guard("fetch_new", |p| p.fetch_new(after_uid))
    }

    fn fetch_new_descriptors(
        &mut self,
        after_uid: Option<i64>,
    ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
        self.guard("fetch_new_descriptors", |p| {
            p.fetch_new_descriptors(after_uid)
        })
    }

    fn fetch_descriptor_chunk(
        &mut self,
        mailbox_path: &str,
        low: i64,
        high: i64,
    ) -> Result<DescriptorChunkResult, ProviderError> {
        self.guard("fetch_descriptor_chunk", |p| {
            p.fetch_descriptor_chunk(mailbox_path, low, high)
        })
    }

    fn search_uids_in(&mut self, low: i64, high: i64) -> Result<Vec<i64>, ProviderError> {
        self.guard("search_uids_in", |p| p.search_uids_in(low, high))
    }

    fn fetch_descriptor_list(
        &mut self,
        mailbox_path: &str,
        uids: &[i64],
    ) -> Result<DescriptorChunkResult, ProviderError> {
        self.guard("fetch_descriptor_list", |p| {
            p.fetch_descriptor_list(mailbox_path, uids)
        })
    }

    fn retry_descriptor_skips(
        &mut self,
        mailbox_path: &str,
        uids: &[i64],
    ) -> Result<DescriptorChunkResult, ProviderError> {
        self.guard("retry_descriptor_skips", |p| {
            p.retry_descriptor_skips(mailbox_path, uids)
        })
    }

    fn first_uid_since(&mut self, imap_date: &str) -> Result<Option<i64>, ProviderError> {
        self.guard("first_uid_since", |p| p.first_uid_since(imap_date))
    }

    fn fetch_bodies(&mut self, uids: &[i64]) -> Result<Vec<RemoteMessage>, ProviderError> {
        self.guard("fetch_bodies", |p| p.fetch_bodies(uids))
    }

    fn fetch_attachment_part(&mut self, uid: i64, section: &str) -> Result<Vec<u8>, ProviderError> {
        self.guard("fetch_attachment_part", |p| {
            p.fetch_attachment_part(uid, section)
        })
    }

    fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
        self.guard("list_uid_flags", |p| p.list_uid_flags())
    }

    /// Abandoned by the guard itself (a caught panic) OR by the wrapped
    /// provider (e.g. an IMAP session poisoned by an undecodable response
    /// with no reconnect left — review of PR #180, finding 2). Reporting only
    /// the guard's own flag hid the inner provider's state from the sync
    /// engine, which then failed every later mailbox at SELECT.
    fn session_abandoned(&self) -> bool {
        self.is_abandoned() || self.inner.session_abandoned()
    }

    ///: a panic mid-protocol is itself a decode-shaped
    /// failure (an unparseable/unexpected response desynchronised the
    /// stream, same family as an undecodable response that poisons a
    /// `Session`) — so it reads as [`SessionAbandonReason::Poisoned`],
    /// same as before this method existed. Otherwise defers to the wrapped
    /// provider's own reason.
    fn session_abandon_reason(&self) -> Option<SessionAbandonReason> {
        if self.is_abandoned() {
            Some(SessionAbandonReason::Poisoned)
        } else {
            self.inner.session_abandon_reason()
        }
    }

    /// Read-only accessor over cached data; it issues no command, so there is
    /// nothing to desynchronise and no guard is needed.
    fn capabilities(&self) -> Option<Vec<String>> {
        self.inner.capabilities()
    }

    fn logout(&mut self) -> Result<(), ProviderError> {
        self.guard("logout", |p| p.logout())
    }

    fn store_seen(
        &mut self,
        mailbox_path: &str,
        uid: i64,
        seen: bool,
    ) -> Result<(), ProviderError> {
        self.guard("store_seen", |p| p.store_seen(mailbox_path, uid, seen))
    }

    fn mark_all_seen(&mut self, mailbox_path: &str) -> Result<(), ProviderError> {
        self.guard("mark_all_seen", |p| p.mark_all_seen(mailbox_path))
    }

    fn mark_seen_uids(&mut self, mailbox_path: &str, uids: &str) -> Result<(), ProviderError> {
        self.guard("mark_seen_uids", |p| p.mark_seen_uids(mailbox_path, uids))
    }

    fn store_flagged(
        &mut self,
        mailbox_path: &str,
        uid: i64,
        flagged: bool,
    ) -> Result<(), ProviderError> {
        self.guard("store_flagged", |p| {
            p.store_flagged(mailbox_path, uid, flagged)
        })
    }

    fn store_keyword(
        &mut self,
        mailbox_path: &str,
        uid: i64,
        keyword: &str,
        add: bool,
    ) -> Result<KeywordStoreOutcome, ProviderError> {
        self.guard("store_keyword", |p| {
            p.store_keyword(mailbox_path, uid, keyword, add)
        })
    }

    fn move_message(
        &mut self,
        src_path: &str,
        uid: i64,
        message_id: &str,
        dest_path: &str,
    ) -> Result<(), ProviderError> {
        self.guard("move_message", |p| {
            p.move_message(src_path, uid, message_id, dest_path)
        })
    }

    fn locate_by_message_id(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
    ) -> Result<Vec<i64>, ProviderError> {
        self.guard("locate_by_message_id", |p| {
            p.locate_by_message_id(mailbox_path, message_id)
        })
    }

    fn expunge_mailbox(&mut self, mailbox_path: &str, uids: &str) -> Result<(), ProviderError> {
        self.guard("expunge_mailbox", |p| p.expunge_mailbox(mailbox_path, uids))
    }

    fn append(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
        raw: &[u8],
    ) -> Result<(), ProviderError> {
        self.guard("append", |p| p.append(mailbox_path, message_id, raw))
    }

    /// Read-only accessor over cached negotiation state — no command issued,
    /// nothing to desynchronise, matches `capabilities()`'s no-guard stance.
    fn qresync_available(&self) -> bool {
        self.inner.qresync_available()
    }

    /// Read-only accessor over the last `select`'s own response — same
    /// reasoning as `qresync_available`.
    fn last_highest_modseq(&self) -> Option<i64> {
        self.inner.last_highest_modseq()
    }

    fn select_qresync(
        &mut self,
        mailbox_path: &str,
        uid_validity: i64,
        highest_modseq: i64,
    ) -> Result<QresyncSync, ProviderError> {
        self.guard("select_qresync", |p| {
            p.select_qresync(mailbox_path, uid_validity, highest_modseq)
        })
    }

    /// Read-only accessor over cached capability state — no command issued,
    /// nothing to desynchronise, matches `capabilities()`'s no-guard stance.
    fn idle_supported(&self) -> bool {
        self.inner.idle_supported()
    }

    fn idle_start(&mut self, mailbox_path: &str) -> Result<(), ProviderError> {
        self.guard("idle_start", |p| p.idle_start(mailbox_path))
    }

    fn idle_wait(&mut self, timeout: std::time::Duration) -> Result<IdleWake, ProviderError> {
        self.guard("idle_wait", |p| p.idle_wait(timeout))
    }

    fn idle_done(&mut self) -> Result<(), ProviderError> {
        self.guard("idle_done", |p| p.idle_done())
    }

    // S36: without these three overrides, calls fall through to the trait's
    // default `Unsupported` impls instead of `self.inner`'s real one — every
    // `PanicGuardedProvider<ImapProvider>` (i.e. every real replay) would
    // silently refuse every server-draft save/cleanup. Caught by
    // `server_draft_write_methods_forward_through_the_guard` below.
    fn append_with_flags_get_uid(
        &mut self,
        mailbox_path: &str,
        flags: &[MessageFlag],
        raw: &[u8],
    ) -> Result<Option<AppendedUid>, ProviderError> {
        self.guard("append_with_flags_get_uid", |p| {
            p.append_with_flags_get_uid(mailbox_path, flags, raw)
        })
    }

    fn find_uid_by_message_id(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
    ) -> Result<Option<AppendedUid>, ProviderError> {
        self.guard("find_uid_by_message_id", |p| {
            p.find_uid_by_message_id(mailbox_path, message_id)
        })
    }

    fn delete_uid(&mut self, mailbox_path: &str, uid: i64) -> Result<(), ProviderError> {
        self.guard("delete_uid", |p| p.delete_uid(mailbox_path, uid))
    }

    // The `*_checked` writes are forwarded explicitly: the trait defaults
    // would work, but they SELECT-then-write in two steps, discarding the
    // inner provider's own same-SELECT UIDVALIDITY compare.

    fn store_seen_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        seen: bool,
    ) -> Result<Guarded<()>, ProviderError> {
        self.guard("store_seen_checked", |p| {
            p.store_seen_checked(mailbox_path, expected_uid_validity, uid, seen)
        })
    }

    fn store_flagged_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        flagged: bool,
    ) -> Result<Guarded<()>, ProviderError> {
        self.guard("store_flagged_checked", |p| {
            p.store_flagged_checked(mailbox_path, expected_uid_validity, uid, flagged)
        })
    }

    fn store_keyword_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        keyword: &str,
        add: bool,
    ) -> Result<Guarded<KeywordStoreOutcome>, ProviderError> {
        self.guard("store_keyword_checked", |p| {
            p.store_keyword_checked(mailbox_path, expected_uid_validity, uid, keyword, add)
        })
    }

    fn move_message_reporting(
        &mut self,
        src_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        message_id: &str,
        dest_path: &str,
    ) -> Result<Guarded<Option<MovedTo>>, ProviderError> {
        self.guard("move_message_reporting", |p| {
            p.move_message_reporting(src_path, expected_uid_validity, uid, message_id, dest_path)
        })
    }

    fn move_message_checked(
        &mut self,
        src_path: &str,
        expected_uid_validity: i64,
        uid: i64,
        message_id: &str,
        dest_path: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        self.guard("move_message_checked", |p| {
            p.move_message_checked(src_path, expected_uid_validity, uid, message_id, dest_path)
        })
    }

    fn delete_uid_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uid: i64,
    ) -> Result<Guarded<()>, ProviderError> {
        self.guard("delete_uid_checked", |p| {
            p.delete_uid_checked(mailbox_path, expected_uid_validity, uid)
        })
    }

    fn expunge_mailbox_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        self.guard("expunge_mailbox_checked", |p| {
            p.expunge_mailbox_checked(mailbox_path, expected_uid_validity, uids)
        })
    }

    fn mark_seen_uids_checked(
        &mut self,
        mailbox_path: &str,
        expected_uid_validity: i64,
        uids: &str,
    ) -> Result<Guarded<()>, ProviderError> {
        self.guard("mark_seen_uids_checked", |p| {
            p.mark_seen_uids_checked(mailbox_path, expected_uid_validity, uids)
        })
    }

    fn find_uids_by_message_id(
        &mut self,
        mailbox_path: &str,
        message_id: &str,
    ) -> Result<MessageIdMatches, ProviderError> {
        self.guard("find_uids_by_message_id", |p| {
            p.find_uids_by_message_id(mailbox_path, message_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UidRangeSet;

    /// A provider that panics on demand, standing in for the imap/imap-proto
    /// panics we cannot fix from here.
    struct Exploding {
        panic_on_select: bool,
        panic_on_select_qresync: bool,
        panic_on_idle_wait: bool,
        panic_on_store_keyword: bool,
        selects: usize,
        lists: usize,
    }

    impl MailProvider for Exploding {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            self.lists += 1;
            Ok(vec![])
        }

        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            self.selects += 1;
            if self.panic_on_select {
                panic!("assertion `left == right` failed: server bytes 0x41 0x42");
            }
            Ok(MailboxState {
                uid_validity: 1,
                uid_next: 2,
                exists: 0,
            })
        }

        fn fetch_new(
            &mut self,
            _after_uid: Option<i64>,
        ) -> Result<Vec<RemoteMessage>, ProviderError> {
            Ok(vec![])
        }

        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }

        fn select_qresync(
            &mut self,
            _mailbox_path: &str,
            _uid_validity: i64,
            _highest_modseq: i64,
        ) -> Result<QresyncSync, ProviderError> {
            if self.panic_on_select_qresync {
                panic!("assertion `left == right` failed: server bytes 0x51 0x52");
            }
            Ok(QresyncSync {
                state: MailboxState {
                    uid_validity: 1,
                    uid_next: 2,
                    exists: 0,
                },
                highest_modseq: Some(100),
                changed: vec![],
                vanished: UidRangeSet::default(),
            })
        }

        fn idle_supported(&self) -> bool {
            true
        }

        fn idle_start(&mut self, _mailbox_path: &str) -> Result<(), ProviderError> {
            Ok(())
        }

        fn idle_wait(&mut self, _timeout: std::time::Duration) -> Result<IdleWake, ProviderError> {
            if self.panic_on_idle_wait {
                panic!("assertion `left == right` failed: server bytes 0x49 0x44");
            }
            Ok(IdleWake::Timeout)
        }

        fn idle_done(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }

        fn store_keyword(
            &mut self,
            _mailbox_path: &str,
            _uid: i64,
            _keyword: &str,
            _add: bool,
        ) -> Result<KeywordStoreOutcome, ProviderError> {
            if self.panic_on_store_keyword {
                panic!("assertion `left == right` failed: server bytes 0x61 0x62");
            }
            Ok(KeywordStoreOutcome::Stored)
        }
    }

    fn exploding(panic_on_select: bool) -> PanicGuardedProvider<Exploding> {
        PanicGuardedProvider::new(Exploding {
            panic_on_select,
            panic_on_select_qresync: false,
            panic_on_idle_wait: false,
            panic_on_store_keyword: false,
            selects: 0,
            lists: 0,
        })
    }

    #[test]
    fn a_provider_panic_becomes_a_protocol_error() {
        let mut p = exploding(true);
        let err = p
            .select("INBOX")
            .expect_err("panic must surface as an error");
        match err {
            ProviderError::Protocol(msg) => {
                assert!(msg.contains("select"), "names the operation: {msg}");
                assert!(msg.contains("abandoned"), "says the session is gone: {msg}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn panic_guard_wraps_store_keyword() {
        // Mirrors `a_provider_panic_becomes_a_protocol_error`: the new
        // store_keyword write path must be wrapped by PanicGuardedProvider
        // exactly like every other MailProvider write method.
        let mut p = PanicGuardedProvider::new(Exploding {
            panic_on_select: false,
            panic_on_select_qresync: false,
            panic_on_idle_wait: false,
            panic_on_store_keyword: true,
            selects: 0,
            lists: 0,
        });
        let err = p
            .store_keyword("INBOX", 9, "Work", true)
            .expect_err("panic must surface as an error");
        match err {
            ProviderError::Protocol(msg) => {
                assert!(msg.contains("store_keyword"), "names the operation: {msg}");
                assert!(msg.contains("abandoned"), "says the session is gone: {msg}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn the_panic_payload_never_leaks_server_bytes_into_the_error() {
        // Panic messages from these parsers embed raw server data; the error we
        // hand upward (and may log or show) must carry none of it.
        let mut p = exploding(true);
        let err = p.select("INBOX").expect_err("error");
        let ProviderError::Protocol(msg) = err else {
            panic!("expected Protocol")
        };
        assert!(!msg.contains("0x41"), "leaked server bytes: {msg}");
        assert!(!msg.contains("assertion"), "leaked panic payload: {msg}");
    }

    #[test]
    fn the_session_is_abandoned_after_a_panic_and_never_reused() {
        let mut p = exploding(true);
        let _ = p.select("INBOX");
        assert!(p.is_abandoned());

        // A later call must fail fast WITHOUT touching the desynchronised
        // session — continuing to issue commands is what turned one bad
        // response into a cascade in the Gmail pilot crash.
        let err = p
            .list_mailboxes()
            .expect_err("calls after a panic must fail fast");
        match err {
            ProviderError::Protocol(msg) => assert!(msg.contains("reconnect"), "{msg}"),
            other => panic!("expected Protocol, got {other:?}"),
        }
        assert_eq!(p.inner.lists, 0, "the inner provider must not be called");
    }

    #[test]
    fn a_panic_during_connect_becomes_a_protocol_error_instead_of_unwinding() {
        // `connect` runs LOGIN + CAPABILITY, so it reaches the same parser
        // sites as any later command. Before `connect_with`, a panic there
        // unwound past the caller's error handling and the account's sync
        // failure was never recorded.
        let Err(err) = PanicGuardedProvider::<Exploding>::connect_with(|| {
            panic!("assertion `left == right` failed: server bytes 0x41 0x42")
        }) else {
            panic!("a panic during connect must surface as an error");
        };
        match err {
            ProviderError::Protocol(msg) => {
                assert!(msg.contains("connect"), "names the operation: {msg}");
                assert!(!msg.contains("0x41"), "leaked server bytes: {msg}");
                assert!(!msg.contains("assertion"), "leaked panic payload: {msg}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn connect_with_passes_a_real_connect_error_through_and_wraps_a_success() {
        let Err(err) = PanicGuardedProvider::<Exploding>::connect_with(|| {
            Err(ProviderError::Network("connection refused".into()))
        }) else {
            panic!("a real connect failure stays a connect failure");
        };
        match err {
            ProviderError::Network(msg) => assert!(msg.contains("refused"), "{msg}"),
            other => panic!("expected Network, got {other:?}"),
        }

        // On success the provider comes back already wrapped, so the guard
        // covers every later command too.
        let Ok(mut guarded) = PanicGuardedProvider::connect_with(|| {
            Ok(Exploding {
                panic_on_select: true,
                panic_on_select_qresync: false,
                panic_on_idle_wait: false,
                panic_on_store_keyword: false,
                selects: 0,
                lists: 0,
            })
        }) else {
            panic!("connect");
        };
        assert!(!guarded.is_abandoned());
        assert!(guarded.select("INBOX").is_err(), "guarded after connect");
        assert!(guarded.is_abandoned());
    }

    #[test]
    fn session_abandoned_also_reports_the_wrapped_providers_own_abandonment() {
        struct Abandons(bool);
        impl MailProvider for Abandons {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _: &str) -> Result<MailboxState, ProviderError> {
                self.0 = true;
                Err(ProviderError::Protocol(
                    "poisoned, no reconnect left".into(),
                ))
            }
            fn fetch_new(&mut self, _: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn session_abandoned(&self) -> bool {
                self.0
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
        }

        let mut p = PanicGuardedProvider::new(Abandons(false));
        assert!(!p.session_abandoned());
        let _ = p.select("INBOX");
        assert!(
            p.session_abandoned(),
            "the inner provider abandoned its session; the guard must say so"
        );
        assert!(!p.is_abandoned(), "no panic was caught");
    }

    #[test]
    fn a_healthy_provider_is_passed_through_untouched() {
        let mut p = exploding(false);
        assert!(p.list_mailboxes().is_ok());
        let state = p.select("INBOX").expect("select");
        assert_eq!(state.uid_validity, 1);
        assert!(!p.is_abandoned());
        assert_eq!(p.inner.selects, 1);
        assert!(p.logout().is_ok());
    }

    #[test]
    fn fetch_attachment_part_forwards_through_the_guard() {
        struct Fixed;
        impl MailProvider for Fixed {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                })
            }
            fn fetch_new(
                &mut self,
                _after: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
            fn fetch_attachment_part(
                &mut self,
                uid: i64,
                section: &str,
            ) -> Result<Vec<u8>, ProviderError> {
                assert_eq!(uid, 7);
                assert_eq!(section, "2.2");
                Ok(b"hello".to_vec())
            }
        }
        let mut guarded = PanicGuardedProvider::new(Fixed);
        let bytes = guarded
            .fetch_attachment_part(7, "2.2")
            .expect("forwards to Fixed");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn a_select_qresync_panic_abandons_the_session_like_select_does() {
        let mut p = PanicGuardedProvider::new(Exploding {
            panic_on_select: false,
            panic_on_select_qresync: true,
            panic_on_idle_wait: false,
            panic_on_store_keyword: false,
            selects: 0,
            lists: 0,
        });
        let err = p
            .select_qresync("INBOX", 1, 100)
            .expect_err("panic must surface as an error");
        match err {
            ProviderError::Protocol(msg) => {
                assert!(msg.contains("select_qresync"), "{msg}");
                assert!(msg.contains("abandoned"), "{msg}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
        assert!(p.is_abandoned());
    }

    #[test]
    fn an_idle_wait_panic_abandons_the_session() {
        let mut p = PanicGuardedProvider::new(Exploding {
            panic_on_select: false,
            panic_on_select_qresync: false,
            panic_on_idle_wait: true,
            panic_on_store_keyword: false,
            selects: 0,
            lists: 0,
        });
        let err = p
            .idle_wait(std::time::Duration::from_millis(1))
            .expect_err("panic must surface as an error");
        assert!(matches!(err, ProviderError::Protocol(_)));
        assert!(p.is_abandoned());
    }

    /// S36's three server-draft write methods have default trait impls that
    /// return `Unsupported` — a `PanicGuardedProvider` that doesn't override
    /// them would silently mask a real provider's implementation behind
    /// that default, exactly the same trap `fetch_attachment_part_forwards_
    /// through_the_guard` above already guards against for an older method.
    #[test]
    fn server_draft_write_methods_forward_through_the_guard() {
        struct Fixed;
        impl MailProvider for Fixed {
            fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
                Ok(vec![])
            }
            fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
                Ok(MailboxState {
                    uid_validity: 1,
                    uid_next: 1,
                    exists: 0,
                })
            }
            fn fetch_new(
                &mut self,
                _after: Option<i64>,
            ) -> Result<Vec<RemoteMessage>, ProviderError> {
                Ok(vec![])
            }
            fn logout(&mut self) -> Result<(), ProviderError> {
                Ok(())
            }
            fn append_with_flags_get_uid(
                &mut self,
                mailbox_path: &str,
                flags: &[MessageFlag],
                raw: &[u8],
            ) -> Result<Option<AppendedUid>, ProviderError> {
                assert_eq!(mailbox_path, "Drafts");
                assert_eq!(flags, &[MessageFlag::Draft]);
                assert_eq!(raw, b"raw");
                Ok(Some(AppendedUid {
                    uid_validity: 1,
                    uid: 9,
                }))
            }
            fn find_uid_by_message_id(
                &mut self,
                mailbox_path: &str,
                message_id: &str,
            ) -> Result<Option<AppendedUid>, ProviderError> {
                assert_eq!(mailbox_path, "Drafts");
                assert_eq!(message_id, "<m1@x>");
                Ok(Some(AppendedUid {
                    uid_validity: 1,
                    uid: 9,
                }))
            }
            fn delete_uid(&mut self, mailbox_path: &str, uid: i64) -> Result<(), ProviderError> {
                assert_eq!(mailbox_path, "Drafts");
                assert_eq!(uid, 9);
                Ok(())
            }
        }
        let mut guarded = PanicGuardedProvider::new(Fixed);
        let appended = guarded
            .append_with_flags_get_uid("Drafts", &[MessageFlag::Draft], b"raw")
            .expect("forwards to Fixed")
            .expect("Fixed returns Some");
        assert_eq!(appended.uid, 9);
        let found = guarded
            .find_uid_by_message_id("Drafts", "<m1@x>")
            .expect("forwards to Fixed")
            .expect("Fixed returns Some");
        assert_eq!(found.uid, 9);
        guarded.delete_uid("Drafts", 9).expect("forwards to Fixed");
    }
}
