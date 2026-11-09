// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Explainable communication-health signals, computed on demand from
//! existing tables (`messages`, `message_addresses`, `accounts`,
//! `work_items`) -- no migration, nothing stored.
//!
//! Every [`HealthSignal`] carries its numeric `value`, a human-readable
//! `explanation` of how that number came about, and `evidence_ids` (message
//! ids, or work-item ids for the waiting signal). Evidence is ids ONLY: no
//! subjects, snippets or bodies ever leave this module.
//!
//! Direction rule (From/Date/Message-ID are sender-controlled, so headers alone
//! never decide). With `from_self` = From is one of the profile's account emails
//! (or a contact flagged `is_self`) and `in_sent` = the copy sits in a
//! `role='sent'` mailbox:
//! - `from_self && in_sent`   -> outbound.
//! - `!from_self && !in_sent` -> inbound (incl. mailboxes with NULL/unknown role).
//! - `from_self && !in_sent`  -> IGNORED: self-sent/forged mail in INBOX, or a
//!   Gmail All Mail copy. We cannot prove it is ours, and counting it either way
//!   is unsafe (outbound would hide unanswered mail; inbound is a false alarm).
//!   A genuine sent copy still counts via its `sent`-role sibling.
//! - `!from_self && in_sent`  -> IGNORED: a foreign message in a Sent folder
//!   (we did not write it; it must not become an "unanswered" inbound).
//!
//! Drafts (flag or `drafts`/`outbox` role), deleted, junk/trash and future-dated
//! messages are ignored. Copies of one email in several mailboxes count once,
//! keyed by (account, sender, sent_at, Message-ID, else row id), so a forged
//! duplicate Message-ID from a different sender never collapses distinct mail.
//!
//! Signals (stable `key`s, in this order):
//! - `reply_latency`    median seconds from an inbound message to the first
//!   later outbound message in the same thread (answered inbound only).
//! - `cadence_gap`      silence since the last message vs the historical
//!   median gap between consecutive messages (needs >= 3 messages).
//! - `unanswered_inbound`  inbound messages with no later outbound reply in
//!   their thread (messages without a thread: no later outbound at all).
//! - `waiting_since_age`   age in seconds of the oldest `waiting` work item
//!   linked to the subject (`work_items.waiting_since`).
//!
//! Clock convention: `*_at(.., now)` take the epoch explicitly so tests are
//! deterministic; the core wrappers supply the wall clock.

use rusqlite::{named_params, params};
use serde::Serialize;

use crate::{Database, DbError, EntityKind};

/// Evidence ids returned per signal are capped (most recent first).
pub const EVIDENCE_CAP: usize = 10;

/// One explainable signal.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HealthSignal {
    /// Stable machine key (see module docs).
    pub key: &'static str,
    /// The measured value; `None` when there is not enough history.
    pub value: Option<f64>,
    /// "seconds" | "count" | "ratio".
    pub unit: &'static str,
    /// Human-readable derivation of `value`.
    pub explanation: String,
    /// "message" | "work_item".
    pub evidence_kind: &'static str,
    /// Ids backing the value, most recent first, capped at [`EVIDENCE_CAP`].
    pub evidence_ids: Vec<String>,
}

/// All signals for one contact or thread at `as_of`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommunicationHealth {
    /// "contact" | "thread".
    pub subject_kind: &'static str,
    pub subject_id: String,
    pub as_of: i64,
    pub signals: Vec<HealthSignal>,
}

/// A direction-tagged message event (input of the pure computation).
#[derive(Clone, Debug, PartialEq)]
pub struct MailEvent {
    pub id: String,
    pub thread_id: Option<String>,
    pub at: i64,
    pub outbound: bool,
}

/// A waiting work item (input of the pure computation).
#[derive(Clone, Debug, PartialEq)]
pub struct WaitingItem {
    pub id: String,
    pub waiting_since: i64,
}

fn fmt_dur(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 86_400 {
        format!("{:.1} days", s as f64 / 86_400.0)
    } else if s >= 3_600 {
        format!("{:.1} hours", s as f64 / 3_600.0)
    } else {
        format!("{} minutes", s / 60)
    }
}

fn median(v: &mut [i64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] + v[n / 2]) as f64 / 2.0
    })
}

/// Most recent first (by the i64 key), ties by id, capped.
fn capped_recent(mut ids: Vec<(i64, String)>) -> Vec<String> {
    ids.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ids.into_iter()
        .take(EVIDENCE_CAP)
        .map(|(_, id)| id)
        .collect()
}

/// Pure signal computation. `events` may be in any order.
pub fn compute_signals(
    events: &[MailEvent],
    waiting: &[WaitingItem],
    now: i64,
) -> Vec<HealthSignal> {
    let mut ev: Vec<&MailEvent> = events.iter().collect();
    ev.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));

    // First later outbound after an inbound, same thread (or, for a message
    // without a thread, anywhere in the subject's history).
    let reply_to = |i: usize| -> Option<&MailEvent> {
        let m = ev[i];
        ev[i + 1..]
            .iter()
            .find(|o| o.outbound && (m.thread_id.is_none() || o.thread_id == m.thread_id))
            .copied()
    };

    let mut latencies = Vec::new();
    let mut latency_evidence = Vec::new();
    let mut unanswered = Vec::new();
    for (i, m) in ev.iter().enumerate().filter(|(_, m)| !m.outbound) {
        match reply_to(i) {
            Some(r) => {
                latencies.push(r.at - m.at);
                latency_evidence.push((m.at, m.id.clone()));
            }
            None => unanswered.push((m.at, m.id.clone())),
        }
    }

    let mut signals = Vec::with_capacity(4);

    // reply_latency
    let n = latencies.len();
    let med = median(&mut latencies);
    signals.push(HealthSignal {
        key: "reply_latency",
        value: med,
        unit: "seconds",
        explanation: match med {
            Some(m) => format!(
                "Median time to reply to an incoming message is {} across {n} answered message(s).",
                fmt_dur(m as i64)
            ),
            None => "No incoming message has been answered yet, so no reply latency.".into(),
        },
        evidence_kind: "message",
        evidence_ids: capped_recent(latency_evidence),
    });

    // cadence_gap
    let (value, explanation, evidence) = match ev.last() {
        Some(last) if ev.len() >= 3 => {
            let mut gaps: Vec<i64> = ev.windows(2).map(|w| w[1].at - w[0].at).collect();
            let typical = median(&mut gaps).unwrap_or(0.0);
            let silence = (now - last.at).max(0);
            if typical > 0.0 {
                let ratio = silence as f64 / typical;
                (
                    Some(ratio),
                    format!(
                        "Silent for {} against a typical gap of {} between messages ({ratio:.1}x), from {} messages.",
                        fmt_dur(silence),
                        fmt_dur(typical as i64),
                        ev.len()
                    ),
                    vec![last.id.clone()],
                )
            } else {
                (
                    None,
                    "Typical gap between messages is zero, so no ratio.".into(),
                    vec![last.id.clone()],
                )
            }
        }
        Some(last) => (
            None,
            format!(
                "Only {} message(s); at least 3 are needed to establish a cadence.",
                ev.len()
            ),
            vec![last.id.clone()],
        ),
        None => (None, "No messages, so no cadence.".into(), Vec::new()),
    };
    signals.push(HealthSignal {
        key: "cadence_gap",
        value,
        unit: "ratio",
        explanation,
        evidence_kind: "message",
        evidence_ids: evidence,
    });

    // unanswered_inbound
    let count = unanswered.len();
    signals.push(HealthSignal {
        key: "unanswered_inbound",
        value: Some(count as f64),
        unit: "count",
        explanation: if count == 0 {
            "Every incoming message has a later reply from you.".into()
        } else {
            format!("{count} incoming message(s) have no later reply from you.")
        },
        evidence_kind: "message",
        evidence_ids: capped_recent(unanswered),
    });

    // waiting_since_age
    let oldest = waiting.iter().min_by(|a, b| {
        a.waiting_since
            .cmp(&b.waiting_since)
            .then_with(|| a.id.cmp(&b.id))
    });
    signals.push(match oldest {
        Some(o) => {
            let age = (now - o.waiting_since).max(0);
            HealthSignal {
                key: "waiting_since_age",
                value: Some(age as f64),
                unit: "seconds",
                explanation: format!(
                    "Waiting on others for {} (oldest of {} waiting work item(s)).",
                    fmt_dur(age),
                    waiting.len()
                ),
                evidence_kind: "work_item",
                // Oldest first is the most relevant evidence here.
                evidence_ids: capped_recent(
                    waiting
                        .iter()
                        .map(|w| (-w.waiting_since, w.id.clone()))
                        .collect(),
                ),
            }
        }
        None => HealthSignal {
            key: "waiting_since_age",
            value: None,
            unit: "seconds",
            explanation: "No work item is currently waiting on others.".into(),
            evidence_kind: "work_item",
            evidence_ids: Vec::new(),
        },
    });
    signals
}

/// Shared SQL; see the module docs for the direction rule. `dedupe_key` is
/// scoped to account + sender + sent_at + Message-ID so only genuine copies of
/// one email (INBOX + All Mail) collapse; it is never sender-spoofable across
/// different senders.
const EVENT_SELECT: &str = "SELECT m.id, m.thread_id, m.sent_at,
        (lower(COALESCE(m.from_address, '')) IN
            (SELECT lower(email) FROM accounts
             UNION SELECT lower(email) FROM contacts WHERE is_self = 1)) AS from_self,
        (COALESCE(mb.role, '') = 'sent') AS in_sent,
        m.account_id || char(31) || lower(COALESCE(m.from_address, '')) || char(31)
            || m.sent_at || char(31) || COALESCE(m.rfc822_message_id, m.id) AS dedupe_key
     FROM messages m
     JOIN mailboxes mb ON mb.id = m.mailbox_id
     LEFT JOIN message_flags f ON f.message_id = m.id";
const EVENT_LIVE: &str = "m.sent_at IS NOT NULL AND m.sent_at <= :now
     AND COALESCE(mb.role, '') NOT IN ('junk', 'trash', 'drafts', 'outbox')
     AND COALESCE(f.is_draft, 0) = 0 AND COALESCE(f.is_deleted, 0) = 0";

/// Raw row: event plus the dedupe key.
struct RawEvent {
    ev: MailEvent,
    key: String,
}

/// `None` = ignored by the direction rule (module docs).
fn row_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<RawEvent>> {
    let from_self = r.get::<_, i64>(3)? != 0;
    let in_sent = r.get::<_, i64>(4)? != 0;
    if from_self != in_sent {
        return Ok(None);
    }
    Ok(Some(RawEvent {
        ev: MailEvent {
            id: r.get(0)?,
            thread_id: r.get(1)?,
            at: r.get(2)?,
            outbound: from_self,
        },
        key: r.get(5)?,
    }))
}

/// Collapse copies of one email (same dedupe key) into a single event: the
/// earliest copy by (time, id) wins, and it is outbound if any copy is.
fn dedupe_events(raw: Vec<RawEvent>) -> Vec<MailEvent> {
    let mut by_key: std::collections::HashMap<String, MailEvent> = std::collections::HashMap::new();
    for RawEvent { ev, key } in raw {
        match by_key.get_mut(&key) {
            None => {
                by_key.insert(key, ev);
            }
            Some(cur) => {
                let outbound = cur.outbound || ev.outbound;
                if (ev.at, &ev.id) < (cur.at, &cur.id) {
                    *cur = ev;
                }
                cur.outbound = outbound;
            }
        }
    }
    by_key.into_values().collect()
}

impl Database {
    /// Communication-health signals for one contact at `now`. Inbound =
    /// messages the contact sent; outbound = our messages addressed (to/cc)
    /// to them. `Ok(None)` for an unknown contact id.
    pub fn contact_health_at(
        &self,
        contact_id: i64,
        now: i64,
    ) -> Result<Option<CommunicationHealth>, DbError> {
        let Some(contact) = self.get_contact_detail(contact_id)? else {
            return Ok(None);
        };
        let email = contact.email.to_lowercase();
        let events = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(&format!(
                "{EVENT_SELECT}
                 WHERE {EVENT_LIVE}
                   AND (lower(COALESCE(m.from_address, '')) = :email
                        OR EXISTS (SELECT 1 FROM message_addresses a
                                   WHERE a.message_id = m.id
                                     AND a.kind IN ('to', 'cc')
                                     AND lower(a.address) = :email))"
            ))?;
            let rows = stmt.query_map(named_params! {":email": email, ":now": now}, row_event)?;
            dedupe_events(
                rows.filter_map(Result::transpose)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        let waiting = self.waiting_items(EntityKind::Contact, &contact_id.to_string())?;
        Ok(Some(CommunicationHealth {
            subject_kind: "contact",
            subject_id: contact_id.to_string(),
            as_of: now,
            signals: compute_signals(&events, &waiting, now),
        }))
    }

    /// Communication-health signals for one thread at `now`. `Ok(None)` when
    /// the thread id is unknown.
    pub fn thread_health_at(
        &self,
        thread_id: &str,
        now: i64,
    ) -> Result<Option<CommunicationHealth>, DbError> {
        let (events, exists) = {
            let conn = self.lock_conn();
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?1)",
                params![thread_id],
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(&format!(
                "{EVENT_SELECT} WHERE m.thread_id = :tid AND {EVENT_LIVE}"
            ))?;
            let rows = stmt.query_map(named_params! {":tid": thread_id, ":now": now}, row_event)?;
            (
                dedupe_events(
                    rows.filter_map(Result::transpose)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                exists,
            )
        };
        if !exists {
            return Ok(None);
        }
        let waiting = self.waiting_items(EntityKind::Thread, thread_id)?;
        Ok(Some(CommunicationHealth {
            subject_kind: "thread",
            subject_id: thread_id.to_string(),
            as_of: now,
            signals: compute_signals(&events, &waiting, now),
        }))
    }

    /// Work items in state `waiting` with a `waiting_since`, linked to the
    /// subject (a contact also matches items delegated to it).
    fn waiting_items(&self, kind: EntityKind, id: &str) -> Result<Vec<WaitingItem>, DbError> {
        Ok(self
            .work_items_for(kind, id)?
            .into_iter()
            .filter(|w| w.state == "waiting")
            .filter_map(|w| {
                w.waiting_since.map(|s| WaitingItem {
                    id: w.id,
                    waiting_since: s,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: i64 = 86_400;
    const T0: i64 = 1_800_000_000;

    fn ev(id: &str, thread: Option<&str>, at: i64, outbound: bool) -> MailEvent {
        MailEvent {
            id: id.into(),
            thread_id: thread.map(String::from),
            at,
            outbound,
        }
    }

    fn sig<'a>(s: &'a [HealthSignal], key: &str) -> &'a HealthSignal {
        s.iter().find(|x| x.key == key).unwrap()
    }

    #[test]
    fn empty_history_yields_explained_nulls() {
        let s = compute_signals(&[], &[], T0);
        assert_eq!(s.len(), 4);
        assert_eq!(sig(&s, "reply_latency").value, None);
        assert_eq!(sig(&s, "cadence_gap").value, None);
        assert_eq!(sig(&s, "unanswered_inbound").value, Some(0.0));
        assert_eq!(sig(&s, "waiting_since_age").value, None);
        assert!(s.iter().all(|x| !x.explanation.is_empty()));
    }

    #[test]
    fn latency_is_the_median_and_unanswered_are_counted_per_thread() {
        let events = vec![
            ev("i1", Some("a"), T0, false),
            ev("o1", Some("a"), T0 + 3_600, true), // 1h
            ev("i2", Some("b"), T0 + D, false),
            ev("o2", Some("b"), T0 + D + 3 * 3_600, true), // 3h
            ev("i3", Some("c"), T0 + 2 * D, false),        // unanswered
            ev("o3", Some("a"), T0 + 3 * D, true),         // other thread: no help
        ];
        let s = compute_signals(&events, &[], T0 + 4 * D);
        let lat = sig(&s, "reply_latency");
        assert_eq!(lat.value, Some(7_200.0)); // median of 1h, 3h
        assert!(lat.explanation.contains("2.0 hours"), "{}", lat.explanation);
        assert_eq!(lat.evidence_ids, vec!["i2", "i1"]);
        let un = sig(&s, "unanswered_inbound");
        assert_eq!(un.value, Some(1.0));
        assert_eq!(un.evidence_ids, vec!["i3"]);
    }

    #[test]
    fn threadless_inbound_is_answered_by_any_later_outbound() {
        let events = vec![ev("i", None, T0, false), ev("o", Some("x"), T0 + 60, true)];
        let s = compute_signals(&events, &[], T0 + D);
        assert_eq!(sig(&s, "unanswered_inbound").value, Some(0.0));
        assert_eq!(sig(&s, "reply_latency").value, Some(60.0));
    }

    #[test]
    fn cadence_compares_current_silence_with_the_historical_median_gap() {
        // gaps 7d, 7d -> typical 7d; last message 21d before now -> 3.0x.
        let events = vec![
            ev("m1", Some("t"), T0, false),
            ev("m2", Some("t"), T0 + 7 * D, true),
            ev("m3", Some("t"), T0 + 14 * D, false),
        ];
        let s = compute_signals(&events, &[], T0 + 35 * D);
        let c = sig(&s, "cadence_gap");
        assert_eq!(c.value, Some(3.0));
        assert_eq!(c.evidence_ids, vec!["m3"]);
        assert!(c.explanation.contains("21.0 days"), "{}", c.explanation);
        assert!(c.explanation.contains("7.0 days"), "{}", c.explanation);
        // Two messages are not enough history.
        let s = compute_signals(&events[..2], &[], T0 + 35 * D);
        let c = sig(&s, "cadence_gap");
        assert_eq!(c.value, None);
        assert!(c.explanation.contains("at least 3"));
    }

    #[test]
    fn waiting_age_uses_the_oldest_item_and_reports_ids_only() {
        let waiting = vec![
            WaitingItem {
                id: "w-new".into(),
                waiting_since: T0 + 2 * D,
            },
            WaitingItem {
                id: "w-old".into(),
                waiting_since: T0,
            },
        ];
        let s = compute_signals(&[], &waiting, T0 + 5 * D);
        let w = sig(&s, "waiting_since_age");
        assert_eq!(w.value, Some(5.0 * D as f64));
        assert_eq!(w.evidence_kind, "work_item");
        assert_eq!(w.evidence_ids, vec!["w-old", "w-new"]);
    }

    #[test]
    fn evidence_is_capped() {
        let events: Vec<MailEvent> = (0..30)
            .map(|i| ev(&format!("i{i:02}"), Some("t"), T0 + i, false))
            .collect();
        let s = compute_signals(&events, &[], T0 + D);
        let un = sig(&s, "unanswered_inbound");
        assert_eq!(un.value, Some(30.0));
        assert_eq!(un.evidence_ids.len(), EVIDENCE_CAP);
        assert_eq!(un.evidence_ids[0], "i29");
    }

    // ---- DB-level tests (fixed clocks) ----

    use castellum_models::{Account, Address, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-commhealth-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path).expect("open");
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for (name, role) in [
            ("INBOX", "inbox"),
            ("Sent", "sent"),
            ("Junk", "junk"),
            ("All Mail", "archive"),
        ] {
            db.upsert_mailbox(&Mailbox {
                id: format!("acc1:{name}"),
                account_id: "acc1".into(),
                path: name.into(),
                name: name.into(),
                role: Some(role.into()),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        }
        db
    }

    /// Insert a message; `from`/`to` are plain addresses.
    fn put(db: &Database, mailbox: &str, uid: i64, msgid: &str, at: i64, from: &str, to: &str) {
        let addr = |kind: &str, a: &str| Address {
            kind: kind.into(),
            name: None,
            address: a.into(),
        };
        db.upsert_message(&NewMessage {
            id: format!("acc1:{mailbox}:{uid}"),
            account_id: "acc1".into(),
            mailbox_id: format!("acc1:{mailbox}"),
            provider_uid: Some(uid),
            rfc822_message_id: Some(msgid.into()),
            subject: Some("s".into()),
            from_name: None,
            from_address: Some(from.into()),
            sent_at: Some(at),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![addr("from", from), addr("to", to)],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        })
        .unwrap();
    }

    fn put_mailbox(db: &Database, name: &str, role: Option<&str>) {
        db.upsert_mailbox(&Mailbox {
            id: format!("acc1:{name}"),
            account_id: "acc1".into(),
            path: name.into(),
            name: name.into(),
            role: role.map(String::from),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn unanswered(db: &Database, email: &str, now: i64) -> f64 {
        let cid = contact_id(db, email);
        let h = db.contact_health_at(cid, now).unwrap().unwrap();
        sig(&h.signals, "unanswered_inbound").value.unwrap()
    }

    fn contact_id(db: &Database, email: &str) -> i64 {
        db.refresh_contact_aggregates(T0 + 100 * D).unwrap();
        db.list_contacts(Some(email), true, 10)
            .unwrap()
            .into_iter()
            .find(|c| c.email == email)
            .unwrap()
            .id
    }

    #[test]
    fn forged_self_from_in_inbox_with_future_date_does_not_cancel_unanswered() {
        let db = fresh_db();
        put(&db, "INBOX", 1, "<a@x>", T0, "bob@x.test", "me@local.test");
        // Forgery: From = me, in INBOX, dated far in the future.
        put(
            &db,
            "INBOX",
            2,
            "<f@x>",
            T0 + 50 * D,
            "me@local.test",
            "bob@x.test",
        );
        let cid = contact_id(&db, "bob@x.test");
        let h = db.contact_health_at(cid, T0 + 10 * D).unwrap().unwrap();
        let un = sig(&h.signals, "unanswered_inbound");
        assert_eq!(un.value, Some(1.0), "{}", un.explanation);
        assert_eq!(un.evidence_ids, vec!["acc1:INBOX:1"]);
        // A genuine Sent copy still answers it.
        put(
            &db,
            "Sent",
            3,
            "<r@x>",
            T0 + D,
            "me@local.test",
            "bob@x.test",
        );
        let h = db.contact_health_at(cid, T0 + 10 * D).unwrap().unwrap();
        assert_eq!(sig(&h.signals, "unanswered_inbound").value, Some(0.0));
    }

    #[test]
    fn junk_folder_mail_is_ignored() {
        let db = fresh_db();
        put(&db, "Junk", 1, "<j@x>", T0, "bob@x.test", "me@local.test");
        let cid = contact_id(&db, "bob@x.test");
        let h = db.contact_health_at(cid, T0 + D).unwrap().unwrap();
        assert_eq!(sig(&h.signals, "unanswered_inbound").value, Some(0.0));
        assert_eq!(sig(&h.signals, "cadence_gap").value, None);
    }

    #[test]
    fn same_email_in_two_mailboxes_counts_once() {
        let db = fresh_db();
        // Three distinct emails 7 days apart; the middle one is also in All Mail.
        put(&db, "INBOX", 1, "<m1@x>", T0, "bob@x.test", "me@local.test");
        put(
            &db,
            "INBOX",
            2,
            "<m2@x>",
            T0 + 7 * D,
            "bob@x.test",
            "me@local.test",
        );
        put(
            &db,
            "All Mail",
            2,
            "<m2@x>",
            T0 + 7 * D,
            "bob@x.test",
            "me@local.test",
        );
        put(
            &db,
            "INBOX",
            3,
            "<m3@x>",
            T0 + 14 * D,
            "bob@x.test",
            "me@local.test",
        );
        let cid = contact_id(&db, "bob@x.test");
        let h = db.contact_health_at(cid, T0 + 35 * D).unwrap().unwrap();
        let c = sig(&h.signals, "cadence_gap");
        assert_eq!(c.value, Some(3.0), "{}", c.explanation);
        assert_eq!(sig(&h.signals, "unanswered_inbound").value, Some(3.0));
    }

    #[test]
    fn forged_duplicate_message_id_from_other_sender_does_not_collapse() {
        let db = fresh_db();
        // Two different senders share one Message-ID: both must count.
        put(
            &db,
            "INBOX",
            1,
            "<dup@x>",
            T0,
            "bob@x.test",
            "me@local.test",
        );
        put(
            &db,
            "INBOX",
            2,
            "<dup@x>",
            T0,
            "eve@x.test",
            "me@local.test",
        );
        // A third message from bob carrying the same id and time in All Mail is
        // a genuine copy of the first and still counts once.
        put(
            &db,
            "All Mail",
            1,
            "<dup@x>",
            T0,
            "bob@x.test",
            "me@local.test",
        );
        assert_eq!(unanswered(&db, "bob@x.test", T0 + D), 1.0);
        assert_eq!(unanswered(&db, "eve@x.test", T0 + D), 1.0);
    }

    #[test]
    fn self_from_outside_sent_is_ignored_not_inbound() {
        let db = fresh_db();
        // Self-sent / forged mail in INBOX addressed to bob: neither inbound
        // for bob's view nor an answer.
        put(&db, "INBOX", 1, "<s@x>", T0, "me@local.test", "bob@x.test");
        put(
            &db,
            "INBOX",
            2,
            "<b@x>",
            T0 - D,
            "bob@x.test",
            "me@local.test",
        );
        assert_eq!(unanswered(&db, "bob@x.test", T0 + D), 1.0);
        // Same for an All Mail-only copy (archive role, no sent sibling).
        put(
            &db,
            "All Mail",
            3,
            "<a@x>",
            T0 + 1,
            "me@local.test",
            "bob@x.test",
        );
        assert_eq!(unanswered(&db, "bob@x.test", T0 + D), 1.0);
        // The Sent sibling of that email proves it is ours.
        put(
            &db,
            "Sent",
            4,
            "<a@x>",
            T0 + 1,
            "me@local.test",
            "bob@x.test",
        );
        assert_eq!(unanswered(&db, "bob@x.test", T0 + D), 0.0);
    }

    #[test]
    fn foreign_message_in_sent_folder_is_ignored() {
        let db = fresh_db();
        put(&db, "Sent", 1, "<f@x>", T0, "bob@x.test", "me@local.test");
        assert_eq!(unanswered(&db, "bob@x.test", T0 + D), 0.0);
    }

    #[test]
    fn drafts_outbox_and_unknown_role_mailboxes() {
        let db = fresh_db();
        put_mailbox(&db, "Drafts", Some("drafts"));
        put_mailbox(&db, "Outbox", Some("outbox"));
        put_mailbox(&db, "Weird", None);
        put(&db, "INBOX", 1, "<i@x>", T0, "bob@x.test", "me@local.test");
        // Unsent drafts / queued outbox must not count as a reply.
        put(
            &db,
            "Drafts",
            2,
            "<d@x>",
            T0 + 60,
            "me@local.test",
            "bob@x.test",
        );
        put(
            &db,
            "Outbox",
            3,
            "<o@x>",
            T0 + 60,
            "me@local.test",
            "bob@x.test",
        );
        assert_eq!(unanswered(&db, "bob@x.test", T0 + D), 1.0);
        // NULL-role mailbox: foreign mail is inbound, self-From is not a reply.
        put(
            &db,
            "Weird",
            4,
            "<w@x>",
            T0 + 120,
            "bob@x.test",
            "me@local.test",
        );
        put(
            &db,
            "Weird",
            5,
            "<v@x>",
            T0 + 180,
            "me@local.test",
            "bob@x.test",
        );
        assert_eq!(unanswered(&db, "bob@x.test", T0 + D), 2.0);
    }
}
