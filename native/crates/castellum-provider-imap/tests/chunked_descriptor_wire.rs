// Castellum - chunked descriptor fetch wire tests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `ImapProvider::fetch_descriptor_chunk`/`fetch_descriptor_list` are the
// bounded counterparts of `fetch_new_descriptors` the sync engine uses to
// import a mailbox in ~1000-UID chunks instead of one unbounded
// `UID FETCH n:*` (see castellum-core's `sync.rs`). A single undecodable
// message must never fail the call: the provider isolates it (reconnecting,
// since a decode failure poisons `Session` — CLAUDE.md gotcha 1) and reports
// it as skipped, within a per-pass reconnect budget that always keeps one
// reconnect in reserve to leave a CLEAN session behind.
//
// These tests drive that against `FakeImap`, a small MODEL server (one
// mailbox map shared by every connection, each message good or undecodable)
// rather than a per-connection reply script: the costs under test are
// "how many LOGINs did isolation spend", which a model answers for any
// fetch pattern the provider chooses, while a script would have to predict
// the provider's exact command sequence — and its prefix matching could not
// tell `UID FETCH 3` from `UID FETCH 3:4` anyway.
//
// Run: cargo test -p castellum-provider-imap --test chunked_descriptor_wire

use castellum_provider_imap::ImapProvider;
use castellum_providers::{DescriptorFetchStop, ImapConfig, MailProvider, SessionAbandonReason};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// How one modelled message answers a descriptor `UID FETCH`.
#[derive(Clone, Copy)]
enum Msg {
    /// A well-formed descriptor line.
    Good,
    /// Always the known-undecodable line (`INTERNALDATE NIL`: imap-codec
    /// returns a typed decode error and `Session` poisons itself —
    /// gmail_wire.rs's `an_undecodable_fetch_reply_is_a_typed_error_and_
    /// poisons_the_session`).
    Bad,
    /// Undecodable for the first `n` FETCHes that include it, well-formed
    /// after that: a one-off glitch that does not reproduce.
    BadTimes(u32),
    /// The server sends `* BYE` instead of this message and hangs up, as
    /// Gmail ("System Error") or a throttling O365 can mid-response.
    Bye,
    /// The server closes the TCP connection with NO reply at all — no
    /// `BYE`, nothing — the raw dropped-connection case (a proxy timeout, a
    /// killed connection) distinct from a graceful-looking `BYE` hangup.
    Drop,
}

#[derive(Default)]
struct FakeMailbox {
    uid_validity: u32,
    messages: BTreeMap<u32, Msg>,
}

#[derive(Default)]
struct FakeState {
    mailboxes: HashMap<String, FakeMailbox>,
    /// Successful LOGINs = connections the provider opened (initial + every
    /// reconnect).
    logins: usize,
    /// Every command after its tag, in arrival order across connections.
    commands: Vec<String>,
}

/// One modelled mailbox for `FakeImap::start`: name, UIDVALIDITY, messages.
type MailboxSpec<'a> = (&'a str, u32, Vec<(u32, Msg)>);

/// A model IMAP server bound to 127.0.0.1. Every accepted connection is
/// served on its own thread against the SAME `FakeState`, so a reconnecting
/// client simply finds the same mailboxes again.
struct FakeImap {
    port: u16,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
}

impl FakeImap {
    fn start(mailboxes: Vec<MailboxSpec<'_>>) -> Self {
        let mut state = FakeState::default();
        for (name, uid_validity, messages) in mailboxes {
            state.mailboxes.insert(
                name.to_string(),
                FakeMailbox {
                    uid_validity,
                    messages: messages.into_iter().collect(),
                },
            );
        }
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (accept_state, accept_stop) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if accept_stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { return };
                let state = accept_state.clone();
                std::thread::spawn(move || serve(stream, state));
            }
        });
        FakeImap { port, state, stop }
    }

    fn logins(&self) -> usize {
        self.state.lock().unwrap().logins
    }

    fn commands(&self) -> Vec<String> {
        self.state.lock().unwrap().commands.clone()
    }
}

impl Drop for FakeImap {
    fn drop(&mut self) {
        // Unblock `incoming()` so the accept thread sees the flag and exits.
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Parse an IMAP sequence set (`1:5,9,20:*`) into the UIDs of `existing`
/// it names. `*` is the highest existing UID, and a range is unordered, so
/// `501:*` on a mailbox whose highest UID is 400 names 400 — RFC 3501
/// §6.4.8's "n:* always includes the last message" quirk, reproduced here
/// on purpose.
fn uids_in_set(set: &str, existing: &BTreeMap<u32, Msg>) -> Vec<u32> {
    let max = existing.keys().next_back().copied().unwrap_or(0);
    let value = |s: &str| if s == "*" { max } else { s.parse().unwrap() };
    let mut out: Vec<u32> = Vec::new();
    for part in set.split(',') {
        let (a, b) = match part.split_once(':') {
            Some((a, b)) => (value(a), value(b)),
            None => (value(part), value(part)),
        };
        let (lo, hi) = (a.min(b), a.max(b));
        out.extend(existing.range(lo..=hi).map(|(uid, _)| *uid));
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn good_line(seq: usize, uid: u32) -> String {
    format!(
        "* {seq} FETCH (UID {uid} FLAGS () RFC822.SIZE 100 \
         INTERNALDATE \"01-Jan-2026 00:00:00 +0000\" \
         ENVELOPE (\"Mon, 01 Jan 2026 00:00:00 +0000\" \"Subject {uid}\" \
         ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"sender\" \"example.com\")) \
         ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"recipient\" \"example.com\")) \
         NIL NIL NIL \"<{uid}@example.com>\"))\r\n"
    )
}

fn bad_line(seq: usize, uid: u32) -> String {
    format!("* {seq} FETCH (UID {uid} RFC822.SIZE 10 FLAGS () INTERNALDATE NIL)\r\n")
}

fn serve(stream: TcpStream, state: Arc<Mutex<FakeState>>) {
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let _ = writer.write_all(b"* OK fake IMAP ready\r\n");
    let _ = writer.flush();
    let mut selected: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let (tag, command) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
        let upper = command.to_ascii_uppercase();
        let mut st = state.lock().unwrap();
        st.commands.push(command.to_string());
        let mut out = String::new();
        if upper.starts_with("LOGOUT") {
            let _ = writer.write_all(format!("* BYE bye\r\n{tag} OK LOGOUT done\r\n").as_bytes());
            return;
        } else if upper == "CAPABILITY" {
            out.push_str("* CAPABILITY IMAP4rev1\r\n");
        } else if upper.starts_with("LOGIN ") {
            st.logins += 1;
        } else if let Some(name) = command.strip_prefix("SELECT ") {
            let name = name.trim_matches('"').to_string();
            let Some(mb) = st.mailboxes.get(&name) else {
                let _ = writer.write_all(format!("{tag} NO no such mailbox\r\n").as_bytes());
                continue;
            };
            let next = mb.messages.keys().next_back().map_or(1, |u| u + 1);
            out.push_str(&format!(
                "* FLAGS (\\Seen \\Flagged)\r\n\
                 * OK [UIDVALIDITY {}] ok\r\n\
                 * {} EXISTS\r\n\
                 * OK [UIDNEXT {next}] ok\r\n",
                mb.uid_validity,
                mb.messages.len()
            ));
            selected = Some(name);
        } else if let Some(rest) = upper.strip_prefix("UID FETCH ") {
            let set = rest.split(' ').next().unwrap_or("");
            let name = selected.clone().expect("UID FETCH before SELECT");
            let mb = st.mailboxes.get_mut(&name).unwrap();
            let seqs: HashMap<u32, usize> = mb
                .messages
                .keys()
                .enumerate()
                .map(|(i, uid)| (*uid, i + 1))
                .collect();
            for uid in uids_in_set(set, &mb.messages) {
                let seq = seqs[&uid];
                let msg = mb.messages.get_mut(&uid).unwrap();
                match *msg {
                    Msg::Bye => {
                        out.push_str("* BYE System Error\r\n");
                        let _ = writer.write_all(out.as_bytes());
                        let _ = writer.flush();
                        return;
                    }
                    Msg::Drop => {
                        // Flush whatever came before this message, then
                        // close the socket with no further bytes at all.
                        let _ = writer.write_all(out.as_bytes());
                        let _ = writer.flush();
                        return;
                    }
                    Msg::Good => out.push_str(&good_line(seq, uid)),
                    Msg::Bad => out.push_str(&bad_line(seq, uid)),
                    Msg::BadTimes(0) => out.push_str(&good_line(seq, uid)),
                    Msg::BadTimes(n) => {
                        *msg = Msg::BadTimes(n - 1);
                        out.push_str(&bad_line(seq, uid));
                    }
                }
            }
        } else if let Some(set) = upper.strip_prefix("UID SEARCH UID ") {
            let name = selected.clone().expect("UID SEARCH before SELECT");
            let uids = uids_in_set(set, &st.mailboxes[&name].messages);
            out.push_str("* SEARCH");
            for uid in uids {
                out.push_str(&format!(" {uid}"));
            }
            out.push_str("\r\n");
        }
        drop(st);
        out.push_str(&format!("{tag} OK done\r\n"));
        let _ = writer.write_all(out.as_bytes());
        let _ = writer.flush();
    }
}

fn connect(server: &FakeImap) -> ImapProvider {
    ImapProvider::connect(&ImapConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        username: "pilot".to_string(),
        password: "app-password".to_string(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: false,
    })
    .expect("connect to the fake server")
}

/// `1..=n`, every message good except `bad`.
fn mailbox(n: u32, bad: &[u32]) -> Vec<(u32, Msg)> {
    (1..=n)
        .map(|uid| {
            (
                uid,
                if bad.contains(&uid) {
                    Msg::Bad
                } else {
                    Msg::Good
                },
            )
        })
        .collect()
}

fn uids_of(result: &castellum_providers::DescriptorChunkResult) -> Vec<i64> {
    let mut uids: Vec<i64> = result.descriptors.iter().map(|d| d.uid).collect();
    uids.sort_unstable();
    uids
}

fn skipped_of(result: &castellum_providers::DescriptorChunkResult) -> Vec<i64> {
    let mut uids: Vec<i64> = result.skipped.iter().map(|s| s.uid).collect();
    uids.sort_unstable();
    uids
}

#[test]
fn multiple_chunks_are_fetched_as_bounded_ranges_on_one_connection() {
    let server = FakeImap::start(vec![("INBOX", 7, mailbox(3, &[]))]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let first = provider.fetch_descriptor_chunk("INBOX", 1, 2).unwrap();
    let second = provider.fetch_descriptor_chunk("INBOX", 3, 3).unwrap();
    assert_eq!(uids_of(&first), vec![1, 2]);
    assert_eq!(uids_of(&second), vec![3]);
    assert!(first.skipped.is_empty() && first.incomplete.is_none());
    assert!(second.skipped.is_empty() && second.incomplete.is_none());
    let fetches: Vec<String> = server
        .commands()
        .into_iter()
        .filter(|c| c.starts_with("UID FETCH"))
        .map(|c| c.split(' ').nth(2).unwrap().to_string())
        .collect();
    assert_eq!(
        fetches,
        vec!["1:2", "3"],
        "explicit bounded ranges, never low:*"
    );
    assert_eq!(server.logins(), 1, "no reconnect without a decode failure");
}

#[test]
fn an_empty_range_is_a_no_op_without_a_wire_round_trip() {
    let server = FakeImap::start(vec![("INBOX", 7, mailbox(3, &[]))]);
    let mut provider = connect(&server);
    let result = provider.fetch_descriptor_chunk("INBOX", 5, 3).unwrap();
    assert_eq!(result, Default::default());
    let list = provider.fetch_descriptor_list("INBOX", &[]).unwrap();
    assert_eq!(list, Default::default());
    assert!(
        !server.commands().iter().any(|c| c.starts_with("UID FETCH")),
        "{:?}",
        server.commands()
    );
}

/// Review finding 1 (CRITICAL): one bad UID used to eat the whole 12-reconnect
/// budget (11 bisection nodes + a confirm), so a SECOND bad UID in the same
/// 1000-UID chunk was refused a reconnect, the chunk errored, the first skip
/// was dropped and the checkpoint never moved — the mailbox stalled forever.
/// Isolation now re-fetches in 32-UID sub-batches and fetches single UIDs
/// only inside a failing sub-batch: 1 reconnect for the chunk, then 2 per bad
/// UID (the sub-batch, then the UID itself).
#[test]
fn two_bad_uids_in_one_full_chunk_are_both_isolated_in_a_few_reconnects() {
    let server = FakeImap::start(vec![("INBOX", 9, mailbox(1000, &[300, 700]))]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider
        .fetch_descriptor_chunk("INBOX", 1, 1000)
        .expect("two bad messages must not fail the chunk");

    assert_eq!(skipped_of(&result), vec![300, 700]);
    assert_eq!(result.descriptors.len(), 998);
    assert!(!uids_of(&result).contains(&300) && !uids_of(&result).contains(&700));
    assert!(result.incomplete.is_none(), "{:?}", result.incomplete);
    let reconnects = server.logins() - 1;
    assert_eq!(
        reconnects, 5,
        "1 for the chunk + 2 per bad UID (sub-batch, then the UID alone)"
    );
    assert!(!provider.session_abandoned());

    // Still usable on the same (reconnected, reselected) session.
    let next = provider.fetch_descriptor_chunk("INBOX", 1, 3).unwrap();
    assert_eq!(uids_of(&next), vec![1, 2, 3]);
    assert_eq!(server.logins() - 1, 5, "no further reconnect");
}

/// Two bad UIDs inside the SAME 32-UID sub-batch share its reconnect: one
/// for the chunk, one for the sub-batch, one per bad UID.
#[test]
fn bad_uids_sharing_a_sub_batch_cost_one_reconnect_each_after_the_first() {
    let server = FakeImap::start(vec![("INBOX", 9, mailbox(100, &[40, 41]))]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider.fetch_descriptor_chunk("INBOX", 1, 100).unwrap();
    assert_eq!(skipped_of(&result), vec![40, 41]);
    assert_eq!(result.descriptors.len(), 98);
    assert!(result.incomplete.is_none());
    assert_eq!(server.logins() - 1, 4);
}

#[test]
fn a_one_off_decode_glitch_costs_one_reconnect_and_no_skip() {
    let mut messages = mailbox(40, &[]);
    messages[6].1 = Msg::BadTimes(1); // UID 7: bad once, fine after that
    let server = FakeImap::start(vec![("INBOX", 9, messages)]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider.fetch_descriptor_chunk("INBOX", 1, 40).unwrap();
    assert_eq!(
        result.descriptors.len(),
        40,
        "UID 7 decodes on the re-fetch"
    );
    assert!(
        result.skipped.is_empty(),
        "a failure that does not reproduce is never blamed on a UID"
    );
    assert_eq!(server.logins() - 1, 1);
}

/// Review findings 1 + 2: the budget is 16 reconnects per pass with ONE held
/// in reserve. Isolation stops when a failure would need the reserve, spends
/// the reserve on a clean reconnect + re-SELECT, and returns what it covered
/// so far with a typed stop reason — the sync engine commits that partial
/// progress and moves on. The NEXT mailbox then syncs on a clean session.
/// Only a decode failure with no reconnect left at all abandons the session,
/// explicitly (`session_abandoned()`), without another LOGIN.
#[test]
fn an_exhausted_budget_keeps_partial_progress_and_leaves_a_clean_session() {
    // One bad UID in each of eight different 32-UID sub-batches.
    let bad = [10, 50, 90, 130, 170, 210, 250, 290];
    let server = FakeImap::start(vec![
        ("INBOX", 9, mailbox(1000, &bad)),
        ("Archive", 3, mailbox(5, &[])),
        ("Junk", 4, mailbox(3, &[2])),
    ]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider
        .fetch_descriptor_chunk("INBOX", 1, 1000)
        .expect("a budget stop is a typed partial result, not an error");
    // 1 (chunk) + 7 x 2 (sub-batch + UID) = 15 isolation reconnects; the 8th
    // bad sub-batch [289, 320] then fails with only the reserve left.
    let incomplete = result.incomplete.expect("stopped early");
    assert_eq!(
        incomplete.reason,
        DescriptorFetchStop::ReconnectBudgetExhausted
    );
    assert_eq!(
        incomplete.covered_through,
        Some(288),
        "everything below the failing sub-batch is covered"
    );
    assert_eq!(
        skipped_of(&result),
        bad[..7].iter().map(|&u| i64::from(u)).collect::<Vec<_>>()
    );
    let uids = uids_of(&result);
    assert_eq!(uids.len(), 288 - 7);
    assert!(
        uids.iter().all(|u| *u <= 288),
        "nothing past the stop point"
    );
    assert_eq!(server.logins() - 1, 16, "the reserve was spent on cleanup");
    assert!(!provider.session_abandoned(), "the reserve left it clean");

    // The next mailbox syncs normally on that clean session.
    provider
        .select("Archive")
        .expect("a clean session can SELECT");
    let archive = provider.fetch_descriptor_chunk("Archive", 1, 5).unwrap();
    assert_eq!(uids_of(&archive), vec![1, 2, 3, 4, 5]);
    assert!(archive.incomplete.is_none());

    // A decode failure now has no reconnect left: abandon, explicitly, and
    // never dial again.
    provider.select("Junk").expect("select");
    let junk = provider.fetch_descriptor_chunk("Junk", 1, 3).unwrap();
    assert_eq!(
        junk.incomplete.map(|i| i.reason),
        Some(DescriptorFetchStop::SessionAbandoned)
    );
    assert!(provider.session_abandoned());
    //: the trigger was a decode failure, but the session is
    // abandoned because isolation ran out of a way to recover from it —
    // that reads as `BudgetExhausted` (a network-shaped failure), not
    // `Poisoned`, even though the old, still-poisoned session is what is
    // left in place.
    assert_eq!(
        provider.session_abandon_reason(),
        Some(SessionAbandonReason::BudgetExhausted)
    );
    assert_eq!(server.logins() - 1, 16, "no LOGIN once the budget is gone");
    assert!(
        provider.fetch_descriptor_chunk("Junk", 1, 3).is_err(),
        "an abandoned session is never reused"
    );
}

/// Review of PR #180, round 3: a BYE part-way through a chunk used to come
/// back as a truncated SUCCESS, so the sync engine checkpointed past UIDs
/// that were never fetched and lost them. It is an error now, and the
/// session (the server hung up) is reported abandoned.
#[test]
fn a_bye_mid_chunk_is_an_error_and_never_a_truncated_success() {
    let mut messages = mailbox(10, &[]);
    messages[4].1 = Msg::Bye; // the server hangs up instead of sending UID 5
    let server = FakeImap::start(vec![("INBOX", 9, messages)]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider.fetch_descriptor_chunk("INBOX", 1, 10);
    assert!(
        result.is_err(),
        "UIDs 5-10 were never fetched; this must not look complete: {result:?}"
    );
    assert!(provider.session_abandoned());
    assert_eq!(
        server.logins(),
        1,
        "a hang-up is not a decode failure: no reconnect"
    );
    //: a BYE mid-FETCH closes the session — never a decode
    // failure — so the account must read "network", not "protocol".
    assert_eq!(
        provider.session_abandon_reason(),
        Some(SessionAbandonReason::Closed)
    );
}

///, alongside the BYE case above: the server can also just
/// close the TCP connection with no reply at all (no `* BYE`, nothing) —
/// still a closed connection, never a decode failure, so it must report the
/// same `Closed` reason as a graceful-looking `BYE` hangup.
#[test]
fn the_server_closing_the_socket_mid_chunk_reports_the_closed_abandon_reason() {
    let mut messages = mailbox(10, &[]);
    messages[4].1 = Msg::Drop; // the server hangs up with no reply at all
    let server = FakeImap::start(vec![("INBOX", 9, messages)]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider.fetch_descriptor_chunk("INBOX", 1, 10);
    assert!(
        result.is_err(),
        "UIDs 5-10 were never fetched; this must not look complete: {result:?}"
    );
    assert!(provider.session_abandoned());
    assert_eq!(
        server.logins(),
        1,
        "a dropped connection is not a decode failure: no reconnect"
    );
    assert_eq!(
        provider.session_abandon_reason(),
        Some(SessionAbandonReason::Closed)
    );
}

/// Review round 3: once isolation has started, a failure that is NOT a
/// decode failure (here the server hangs up with a BYE) must not throw away
/// what isolation already covered — including a skip that cost reconnects.
/// It stops with the covered prefix and `Interrupted`.
#[test]
fn an_error_mid_isolation_keeps_the_covered_prefix() {
    let mut messages = mailbox(100, &[40]);
    messages[69].1 = Msg::Bye; // UID 70
    let server = FakeImap::start(vec![("INBOX", 9, messages)]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider
        .fetch_descriptor_chunk("INBOX", 1, 100)
        .expect("isolation already covered 1..=64: keep it");
    let incomplete = result.incomplete.expect("stopped early");
    assert_eq!(incomplete.reason, DescriptorFetchStop::Interrupted);
    assert_eq!(incomplete.covered_through, Some(64));
    assert_eq!(skipped_of(&result), vec![40]);
    assert_eq!(result.descriptors.len(), 63);
    assert!(provider.session_abandoned(), "the server hung up");
}

/// The per-call skip cap (`min(ceil(5%), 20)` of the request) stops the call
/// with the UIDs covered so far instead of failing it: a 20-UID request may
/// skip 1 UID, so the second bad UID ends the call just below itself.
#[test]
fn the_skip_cap_stops_with_partial_progress_just_below_the_next_bad_uid() {
    let server = FakeImap::start(vec![
        ("INBOX", 9, mailbox(20, &[3, 8])),
        ("Archive", 3, mailbox(2, &[])),
    ]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider.fetch_descriptor_chunk("INBOX", 1, 20).unwrap();
    let incomplete = result.incomplete.expect("stopped early");
    assert_eq!(incomplete.reason, DescriptorFetchStop::SkipBudgetExceeded);
    assert_eq!(incomplete.covered_through, Some(7));
    assert_eq!(skipped_of(&result), vec![3]);
    assert_eq!(uids_of(&result), vec![1, 2, 4, 5, 6, 7]);
    assert!(!provider.session_abandoned());
    provider
        .select("Archive")
        .expect("left clean for the next mailbox");
}

/// Review nit 4: `UID SEARCH UID n:*` always names the mailbox's LAST UID,
/// even when it is below n (RFC 3501 §6.4.8). A sparse incremental pass
/// with nothing new used to get that old UID back and re-import it.
#[test]
fn the_search_n_star_last_uid_quirk_is_filtered_to_the_requested_span() {
    let server = FakeImap::start(vec![(
        "INBOX",
        9,
        vec![(100, Msg::Good), (200, Msg::Good), (400, Msg::Good)],
    )]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    assert_eq!(
        provider.search_uids_in(501, 5000).unwrap(),
        Vec::<i64>::new(),
        "400 is below the span even though the server names it"
    );
    assert_eq!(provider.search_uids_in(150, 5000).unwrap(), vec![200, 400]);
    assert_eq!(provider.search_uids_in(150, 300).unwrap(), vec![200]);

    // The provider's OWN filter: imap-codec's default-on sequence-set
    // normalisation re-encodes a range ending at u32::MAX as `501:*`, so the
    // quirk reaches the wire and the server names 400 again.
    assert_eq!(
        provider.search_uids_in(501, i64::from(u32::MAX)).unwrap(),
        Vec::<i64>::new()
    );
    assert!(
        server
            .commands()
            .iter()
            .any(|c| c == "UID SEARCH UID 501:*"),
        "{:?}",
        server.commands()
    );
}

/// Review nit 5: a list FETCH collapses contiguous runs, so a 1000-UID
/// group of 7-8 digit UIDs is not an 8-9 KB command line.
#[test]
fn a_list_fetch_collapses_contiguous_runs_into_ranges() {
    let server = FakeImap::start(vec![("INBOX", 9, mailbox(20, &[]))]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let result = provider
        .fetch_descriptor_list("INBOX", &[9, 1, 2, 3, 4, 5, 10, 20, 3])
        .unwrap();
    assert_eq!(uids_of(&result), vec![1, 2, 3, 4, 5, 9, 10, 20]);
    let fetch = server
        .commands()
        .into_iter()
        .find(|c| c.starts_with("UID FETCH"))
        .unwrap();
    assert!(fetch.starts_with("UID FETCH 1:5,9:10,20 "), "{fetch}");
}

#[test]
fn a_sparse_list_with_a_bad_uid_is_isolated_like_a_range() {
    let messages: Vec<(u32, Msg)> = [501u32, 780, 999, 1500, 2000]
        .iter()
        .map(|&u| (u, if u == 999 { Msg::Bad } else { Msg::Good }))
        .collect();
    let server = FakeImap::start(vec![("INBOX", 9, messages)]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let uids = provider.search_uids_in(501, 2000).unwrap();
    assert_eq!(uids, vec![501, 780, 999, 1500, 2000]);
    let result = provider.fetch_descriptor_list("INBOX", &uids).unwrap();
    assert_eq!(uids_of(&result), vec![501, 780, 1500, 2000]);
    assert_eq!(skipped_of(&result), vec![999]);
    assert!(result.incomplete.is_none());
    assert_eq!(
        server.logins() - 1,
        2,
        "a request that already fits one sub-batch goes straight to single UIDs: \
         1 for the request + 1 for the bad UID"
    );
}

/// Review finding 3: retries of previously-skipped UIDs spend their OWN
/// budget (2 reconnects per pass), one per still-bad UID, and stop before a
/// fetch that could leave a poisoned session with no reconnect to clean it
/// up. The isolation budget is untouched by them.
#[test]
fn retries_spend_their_own_small_budget_and_leave_isolation_untouched() {
    let messages = vec![
        (5, Msg::Bad),
        (6, Msg::Bad),
        (7, Msg::Good),
        (8, Msg::Bad),
        (9, Msg::Good),
    ];
    let server = FakeImap::start(vec![
        ("INBOX", 9, messages),
        (
            "Big",
            9,
            mailbox(1000, &[10, 50, 90, 130, 170, 210, 250, 290]),
        ),
    ]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let retry = provider
        .retry_descriptor_skips("INBOX", &[5, 6, 7])
        .unwrap();
    assert_eq!(skipped_of(&retry), vec![5, 6], "still undecodable");
    assert!(retry.descriptors.is_empty());
    let incomplete = retry.incomplete.expect("the retry budget ran dry");
    assert_eq!(
        incomplete.reason,
        DescriptorFetchStop::ReconnectBudgetExhausted
    );
    assert_eq!(
        incomplete.covered_through,
        Some(6),
        "7 is not attempted with no reconnect left to clean up after it"
    );
    assert_eq!(server.logins() - 1, 2);
    assert!(!provider.session_abandoned());

    // Out of retry budget: nothing is attempted at all.
    let dry = provider.retry_descriptor_skips("INBOX", &[9]).unwrap();
    assert_eq!(dry.incomplete.and_then(|i| i.covered_through), None);
    assert!(dry.descriptors.is_empty());

    // The isolation budget is still WHOLE (16), not merely "not empty":
    // the eight-bad-UID scenario of
    // `an_exhausted_budget_keeps_partial_progress_and_leaves_a_clean_session`
    // spends exactly 16 more LOGINs and stops at exactly the same point.
    provider.select("Big").expect("select");
    let big = provider.fetch_descriptor_chunk("Big", 1, 1000).unwrap();
    let incomplete = big.incomplete.expect("stopped on the budget");
    assert_eq!(
        incomplete.reason,
        DescriptorFetchStop::ReconnectBudgetExhausted
    );
    assert_eq!(incomplete.covered_through, Some(288));
    assert_eq!(server.logins() - 1, 2 + 16);
}

#[test]
fn an_error_mid_retry_keeps_what_was_already_attempted() {
    let messages = vec![(5, Msg::Bad), (6, Msg::Bye), (7, Msg::Good)];
    let server = FakeImap::start(vec![("INBOX", 9, messages)]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let retry = provider
        .retry_descriptor_skips("INBOX", &[5, 6, 7])
        .expect("UID 5 was attempted and is still bad: keep that");
    assert_eq!(skipped_of(&retry), vec![5]);
    let incomplete = retry.incomplete.expect("stopped early");
    assert_eq!(incomplete.reason, DescriptorFetchStop::Interrupted);
    assert_eq!(incomplete.covered_through, Some(5));
    assert!(provider.session_abandoned());
}

#[test]
fn a_retry_that_now_decodes_returns_the_descriptor_without_a_reconnect() {
    let server = FakeImap::start(vec![("INBOX", 9, mailbox(10, &[]))]);
    let mut provider = connect(&server);
    provider.select("INBOX").expect("select");

    let retry = provider.retry_descriptor_skips("INBOX", &[4, 12]).unwrap();
    assert_eq!(uids_of(&retry), vec![4]);
    assert!(retry.skipped.is_empty());
    assert!(
        retry.incomplete.is_none(),
        "12 is simply gone: covered, no descriptor"
    );
    assert_eq!(server.logins(), 1);
}
