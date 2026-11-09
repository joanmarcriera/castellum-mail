// Castellum - shared synthetic corpus, provider and report helpers for the
// perf-gate examples (S64). Copyright 2026 Joan Marc Riera Duocastella.
// SPDX-License-Identifier: Apache-2.0
//
// Included by each perf example with `#[path = "perf_common/mod.rs"] mod
// perf_common;` -- a subdirectory with a `mod.rs` is not picked up by
// cargo's example auto-discovery, so this file is never built as its own
// example and needs no `[[example]]` Cargo.toml entry.
//
// `sync_perf_gate.rs` (PR #150) keeps its own `Check`/`check`/`print_report`
// -- that report's shape is the contract #150 set and decided
// which rows in it block. It only takes this module's `SyntheticProvider`,
// `pct` and (via `SyntheticProvider::single_inbox`) the exact legacy
// single-mailbox descriptor formula, so the #150 table is unchanged.
//
// `seed_profile.rs` and `read_perf_gate.rs` (Task 2/5) use the richer,
// multi-mailbox `corpus_layout`/`synthetic_descriptors`/`SyntheticProvider::new`
// to build the persisted 100k profile and its read-path budgets.
//
// Each example includes its OWN copy of this module (there is no shared
// library crate here), and each only uses a subset of its public API --
// `sync_perf_gate.rs` never touches `Budget`/`judge`, `seed_profile.rs`
// never touches `single_inbox`, and so on. `cargo clippy --all-targets -D
// warnings` runs dead-code analysis per binary, so the unused-in-THIS-
// binary half would otherwise fail the gate; allowed here deliberately,
// the way a real shared-utility module would be.
#![allow(dead_code)]

use castellum_providers::{
    MailProvider, MailboxRoleSource, MailboxState, ProviderError, QresyncSync, RemoteAddress,
    RemoteMailbox, RemoteMessageDescriptor, UidFlags, UidRangeSet,
};

// --- deterministic RNG --------------------------------------------------------

/// xorshift64* -- deterministic, dependency-free (the workspace has no
/// `rand`). Not cryptographic; only used to shape a synthetic corpus.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid the all-zero fixed point.
        Rng(if seed == 0 { 0x9E3779B97F4A7C15 } else { seed })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, n)`. `n == 0` always returns 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

// --- corpus layout -------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct MailboxSpec {
    pub path: &'static str,
    pub role: Option<&'static str>,
    pub count: i64,
}

pub struct AccountSpec {
    pub id: &'static str,
    pub email: &'static str,
    pub mailboxes: Vec<MailboxSpec>,
}

/// The fixed 100k layout: acct-a INBOX 45k / Archive 25k / Sent 10k; acct-b
/// INBOX 15k / Archive 5k. Any other `total` scales every mailbox count
/// proportionally (so `scaled(10_000)` keeps the same ratios) and folds the
/// rounding remainder into the last mailbox so the counts always sum to
/// exactly `total`.
pub fn corpus_layout(total: i64) -> Vec<AccountSpec> {
    const BASE_TOTAL: f64 = 100_000.0;
    let scale = total as f64 / BASE_TOTAL;
    let scaled = |n: i64| -> i64 { (n as f64 * scale).round() as i64 };

    let mut accounts = vec![
        AccountSpec {
            id: "acct-a",
            email: "acct-a@perf.test",
            mailboxes: vec![
                MailboxSpec {
                    path: "INBOX",
                    role: Some("inbox"),
                    count: scaled(45_000),
                },
                MailboxSpec {
                    path: "Archive",
                    role: Some("archive"),
                    count: scaled(25_000),
                },
                MailboxSpec {
                    path: "Sent",
                    role: Some("sent"),
                    count: scaled(10_000),
                },
            ],
        },
        AccountSpec {
            id: "acct-b",
            email: "acct-b@perf.test",
            mailboxes: vec![
                MailboxSpec {
                    path: "INBOX",
                    role: Some("inbox"),
                    count: scaled(15_000),
                },
                MailboxSpec {
                    path: "Archive",
                    role: Some("archive"),
                    count: scaled(5_000),
                },
            ],
        },
    ];

    let summed: i64 = accounts
        .iter()
        .flat_map(|a| a.mailboxes.iter())
        .map(|m| m.count)
        .sum();
    let remainder = total - summed;
    if remainder != 0 {
        // Fold the rounding remainder into the last mailbox of the last
        // account -- keeps every OTHER mailbox's count exactly proportional.
        if let Some(last_account) = accounts.last_mut() {
            if let Some(last_mailbox) = last_account.mailboxes.last_mut() {
                last_mailbox.count += remainder;
            }
        }
    }
    accounts
}

// --- word/sender pools ---------------------------------------------------------

pub const WORD_POOL_SIZE: u64 = 2_000;
const SENDER_POOL_SIZE: u64 = 500;
const DOMAIN_POOL_SIZE: u64 = 50;

pub fn pool_word(idx: u64) -> String {
    format!("word{idx}")
}

/// The pool word `zipfish_word` draws most often: the HIGHEST index (see
/// below), not `pool_word(0)`.
pub fn most_common_word() -> String {
    pool_word(WORD_POOL_SIZE - 1)
}

/// Draw one word index: uniform over `[0, W^2)`, then the integer square
/// root, so the result is always `< W`. Index `k` is hit by the `2k + 1`
/// raw values in `[k^2, (k+1)^2)`, so P(k) = (2k+1)/W^2 -- it grows
/// linearly with the index: `word{W-1}` is the most common word and
/// `word0` the rarest (1 in W^2 draws; absent from a 100k corpus).
pub fn zipfish_word(rng: &mut Rng) -> u64 {
    let raw = rng.below(WORD_POOL_SIZE * WORD_POOL_SIZE);
    (raw as f64).sqrt() as u64
}

fn synthetic_sender(rng: &mut Rng) -> (u64, RemoteAddress) {
    let sender_idx = rng.below(SENDER_POOL_SIZE);
    let domain_idx = sender_idx % DOMAIN_POOL_SIZE;
    (
        domain_idx,
        RemoteAddress {
            name: Some(format!("Sender {sender_idx}")),
            address: format!("sender{sender_idx}@domain{domain_idx}.test"),
        },
    )
}

/// Descriptors for one mailbox: subjects drawn from a 2,000-word pool
/// (Zipf-ish), 500 senders over 50 domains, every 4th message replies
/// (`references`) to an earlier one in the same mailbox, 1 in 11 has
/// attachments, 1 in 3 unread, 1 in 200 has `sent_at = None` (exercises the
/// NULL keyset segment), `sent_at` strictly decreasing with UID but with 1
/// in 50 duplicated timestamps.
pub fn synthetic_descriptors(
    rng: &mut Rng,
    account: &AccountSpec,
    mailbox: &MailboxSpec,
) -> Vec<RemoteMessageDescriptor> {
    const BASE_SENT_AT: i64 = 1_700_000_000;
    const SPACING_SECONDS: i64 = 37;

    let mut out: Vec<RemoteMessageDescriptor> = Vec::with_capacity(mailbox.count as usize);
    let mut last_sent_at = BASE_SENT_AT;
    for i in 0..mailbox.count {
        let uid = i + 1;
        let is_null_sent_at = i > 0 && i % 200 == 0;
        let sent_at = if is_null_sent_at {
            None
        } else if i % 50 == 0 && i > 0 {
            // 1 in 50 duplicates the previous timestamp instead of advancing.
            Some(last_sent_at)
        } else {
            let ts = BASE_SENT_AT - i * SPACING_SECONDS;
            last_sent_at = ts;
            Some(ts)
        };

        let subject_words: Vec<String> = (0..(3 + (i % 4)))
            .map(|_| pool_word(zipfish_word(rng)))
            .collect();
        let subject = format!(
            "Synthetic {} {} #{uid}: {}",
            account.id,
            mailbox.path,
            subject_words.join(" ")
        );

        let (_domain_idx, from) = synthetic_sender(rng);

        let references = if i > 0 && i % 4 == 0 {
            let back = 1 + rng.below((i as u64).min(50));
            let parent_idx = (i as u64).saturating_sub(back);
            vec![format!(
                "<synthetic-{}-{}-{parent_idx}@perf.test>",
                account.id, mailbox.path
            )]
        } else {
            vec![]
        };

        out.push(RemoteMessageDescriptor {
            uid,
            size_bytes: Some(2048),
            has_attachments: Some(i % 11 == 0),
            is_seen: i % 3 != 0,
            is_flagged: uid % 17 == 0,
            rfc822_message_id: Some(format!(
                "<synthetic-{}-{}-{i}@perf.test>",
                account.id, mailbox.path
            )),
            subject: Some(subject),
            from: Some(from),
            to: vec![RemoteAddress {
                name: None,
                address: format!("{}@synthetic.test", account.email),
            }],
            cc: vec![],
            bcc: vec![],
            sent_at,
            references,
            keywords: None,
        });
    }
    out
}

// --- in-memory MailProvider ----------------------------------------------------

struct MailboxEntry {
    remote: RemoteMailbox,
    state: MailboxState,
    descriptors: Vec<RemoteMessageDescriptor>,
}

/// In-memory `MailProvider` over one account's mailboxes. No I/O of any
/// kind. `qresync: true` makes it advertise CONDSTORE/QRESYNC and answer
/// `select_qresync` with an empty delta -- the "nothing changed since last
/// sync" server S30's no-op-resync budget needs.
pub struct SyntheticProvider {
    mailboxes: Vec<MailboxEntry>,
    selected: Option<usize>,
    qresync: bool,
}

/// The fixed HIGHESTMODSEQ this mock reports whenever `qresync` is enabled.
/// Never changes across calls, matching an idle mailbox with nothing new.
const MOCK_HIGHEST_MODSEQ: i64 = 1;

impl SyntheticProvider {
    pub fn new(account: &AccountSpec, rng: &mut Rng, qresync: bool) -> Self {
        let mailboxes = account
            .mailboxes
            .iter()
            .map(|spec| MailboxEntry {
                remote: RemoteMailbox {
                    path: spec.path.to_string(),
                    name: spec.path.to_string(),
                    role: spec.role.map(|r| r.to_string()),
                    role_source: spec.role.map(|_| MailboxRoleSource::SpecialUse),
                    delimiter: Some("/".into()),
                    subscribed: true,
                    selectable: true,
                },
                state: MailboxState {
                    uid_validity: 1,
                    uid_next: spec.count + 1,
                    exists: spec.count,
                },
                descriptors: synthetic_descriptors(rng, account, spec),
            })
            .collect();
        SyntheticProvider {
            mailboxes,
            selected: None,
            qresync,
        }
    }

    /// A single-INBOX provider using the EXACT legacy `sync_perf_gate.rs`
    /// descriptor formula (pre-#S64 refactor): descriptor-only, `uid % 3 !=
    /// 0` seen, `uid % 17 == 0` flagged, `uid % 11 == 0` has attachments,
    /// `sent_at = 1_700_000_000 + uid`, subject `"Synthetic message {uid}"`.
    /// Kept byte-for-byte so #150's budgets measure the same work as before
    /// this refactor (Task 1, Global Constraints).
    pub fn single_inbox(n: i64) -> Self {
        let descriptors: Vec<RemoteMessageDescriptor> = (1..=n)
            .map(|uid| RemoteMessageDescriptor {
                uid,
                size_bytes: Some(2048),
                has_attachments: Some(uid % 11 == 0),
                is_seen: uid % 3 != 0,
                is_flagged: uid % 17 == 0,
                rfc822_message_id: Some(format!("<synthetic-{uid}@perf-gate.test>")),
                subject: Some(format!("Synthetic message {uid}")),
                from: Some(RemoteAddress {
                    name: Some("Synthetic Sender".into()),
                    address: "sender@perf-gate.test".into(),
                }),
                to: vec![RemoteAddress {
                    name: None,
                    address: "recipient@perf-gate.test".into(),
                }],
                cc: vec![],
                bcc: vec![],
                sent_at: Some(1_700_000_000 + uid),
                references: vec![],
                keywords: None,
            })
            .collect();
        SyntheticProvider {
            mailboxes: vec![MailboxEntry {
                remote: RemoteMailbox {
                    path: "INBOX".into(),
                    name: "INBOX".into(),
                    role: Some("inbox".into()),
                    role_source: Some(MailboxRoleSource::SpecialUse),
                    delimiter: Some("/".into()),
                    subscribed: true,
                    selectable: true,
                },
                state: MailboxState {
                    uid_validity: 1,
                    uid_next: n + 1,
                    exists: n,
                },
                descriptors,
            }],
            selected: None,
            qresync: false,
        }
    }

    /// [`Self::single_inbox`], but representing a mailbox that has ALREADY
    /// been synced once at `n` messages (a completed first sync's cursor
    /// covers uid 1..=n) and has since received a small follow-up delta:
    /// `new_count` brand-new messages (uid n+1..=n+new_count, same formula
    /// as `single_inbox`) plus a flag flip on each `(uid, is_seen)` pair in
    /// `flip_seen_to` -- the shape of an ordinary "check for new mail" pass
    /// against an account that already has a large local corpus, not a
    /// first sync. `qresync: false` (RFC 4549 full-scan reconciliation),
    /// matching `single_inbox` and the existing no-op-resync harness, so
    /// this measures the same reconciliation path, not a different one
    /// ('s sync-perf-gate follow-up).
    pub fn single_inbox_with_followup(
        n: i64,
        new_count: i64,
        flip_seen_to: &[(i64, bool)],
    ) -> Self {
        let total = n + new_count;
        let mut descriptors: Vec<RemoteMessageDescriptor> = (1..=total)
            .map(|uid| RemoteMessageDescriptor {
                uid,
                size_bytes: Some(2048),
                has_attachments: Some(uid % 11 == 0),
                is_seen: uid % 3 != 0,
                is_flagged: uid % 17 == 0,
                rfc822_message_id: Some(format!("<synthetic-{uid}@perf-gate.test>")),
                subject: Some(format!("Synthetic message {uid}")),
                from: Some(RemoteAddress {
                    name: Some("Synthetic Sender".into()),
                    address: "sender@perf-gate.test".into(),
                }),
                to: vec![RemoteAddress {
                    name: None,
                    address: "recipient@perf-gate.test".into(),
                }],
                cc: vec![],
                bcc: vec![],
                sent_at: Some(1_700_000_000 + uid),
                references: vec![],
                keywords: None,
            })
            .collect();
        for &(uid, is_seen) in flip_seen_to {
            if let Some(d) = descriptors.iter_mut().find(|d| d.uid == uid) {
                d.is_seen = is_seen;
            }
        }
        SyntheticProvider {
            mailboxes: vec![MailboxEntry {
                remote: RemoteMailbox {
                    path: "INBOX".into(),
                    name: "INBOX".into(),
                    role: Some("inbox".into()),
                    role_source: Some(MailboxRoleSource::SpecialUse),
                    delimiter: Some("/".into()),
                    subscribed: true,
                    selectable: true,
                },
                state: MailboxState {
                    uid_validity: 1,
                    uid_next: total + 1,
                    exists: total,
                },
                descriptors,
            }],
            selected: None,
            qresync: false,
        }
    }

    /// The descriptors this provider serves for `mailbox_path` (empty for an
    /// unknown path). `seed_profile` reads them back to build hydrated
    /// `NewMessage`s whose envelope is IDENTICAL to the descriptor-time row, so
    /// a body-indexed seed changes nothing but the body, snippet and FTS rows.
    pub fn descriptors_of(&self, mailbox_path: &str) -> &[RemoteMessageDescriptor] {
        self.mailboxes
            .iter()
            .find(|m| m.remote.path == mailbox_path)
            .map_or(&[], |m| m.descriptors.as_slice())
    }

    fn selected_entry(&self) -> Option<&MailboxEntry> {
        self.selected.and_then(|i| self.mailboxes.get(i))
    }
}

impl MailProvider for SyntheticProvider {
    fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
        Ok(self.mailboxes.iter().map(|m| m.remote.clone()).collect())
    }

    fn select(&mut self, path: &str) -> Result<MailboxState, ProviderError> {
        let idx = self
            .mailboxes
            .iter()
            .position(|m| m.remote.path == path)
            .ok_or_else(|| ProviderError::Unsupported(format!("no such mailbox: {path}")))?;
        self.selected = Some(idx);
        Ok(self.mailboxes[idx].state)
    }

    fn fetch_new(
        &mut self,
        _after_uid: Option<i64>,
    ) -> Result<Vec<castellum_providers::RemoteMessage>, ProviderError> {
        Err(ProviderError::Unsupported(
            "synthetic perf-gate provider is descriptor-only".into(),
        ))
    }

    fn fetch_new_descriptors(
        &mut self,
        after_uid: Option<i64>,
    ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
        let entry = self
            .selected_entry()
            .ok_or_else(|| ProviderError::Unsupported("no mailbox selected".into()))?;
        Ok(entry
            .descriptors
            .iter()
            .filter(|d| after_uid.is_none_or(|uid| d.uid > uid))
            .cloned()
            .collect())
    }

    fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
        let entry = self
            .selected_entry()
            .ok_or_else(|| ProviderError::Unsupported("no mailbox selected".into()))?;
        Ok(entry
            .descriptors
            .iter()
            .map(|d| UidFlags {
                uid: d.uid,
                is_seen: d.is_seen,
                is_flagged: d.is_flagged,
                keywords: None,
            })
            .collect())
    }

    fn logout(&mut self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn qresync_available(&self) -> bool {
        self.qresync
    }

    fn last_highest_modseq(&self) -> Option<i64> {
        self.qresync.then_some(MOCK_HIGHEST_MODSEQ)
    }

    fn select_qresync(
        &mut self,
        mailbox_path: &str,
        _uid_validity: i64,
        _highest_modseq: i64,
    ) -> Result<QresyncSync, ProviderError> {
        if !self.qresync {
            return Err(ProviderError::Unsupported(
                "this mock does not advertise QRESYNC".into(),
            ));
        }
        let idx = self
            .mailboxes
            .iter()
            .position(|m| m.remote.path == mailbox_path)
            .ok_or_else(|| {
                ProviderError::Unsupported(format!("no such mailbox: {mailbox_path}"))
            })?;
        self.selected = Some(idx);
        Ok(QresyncSync {
            state: self.mailboxes[idx].state,
            highest_modseq: Some(MOCK_HIGHEST_MODSEQ),
            changed: vec![],
            vanished: UidRangeSet::from_ranges([]),
        })
    }
}

// --- budgets/report --------------------------------------------------------------

pub struct Budget {
    pub name: &'static str,
    pub unit: &'static str,
    pub target: f64,
    pub ceiling: f64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
}

/// `<= target` -> Pass; `<= ceiling` -> Warn; else Fail.
pub fn judge(b: &Budget, measured: f64) -> Verdict {
    if measured <= b.target {
        Verdict::Pass
    } else if measured <= b.ceiling {
        Verdict::Warn
    } else {
        Verdict::Fail
    }
}

/// `PERF <name> measured=<v><unit> target=<v> ceiling=<v> <PASS|WARN|FAIL>`
/// -- the one parseable shape every perf line uses (Global Constraints).
///
/// Side effect: when `CASTELLUM_PERF_JSONL` is set the same
/// row is also appended to that file as one JSON object (see `record_row`).
/// Called exactly once per row by the gates, so that is the single hook.
pub fn perf_line(b: &Budget, measured: f64) -> String {
    let verdict = judge(b, measured);
    record_row(b, measured, &verdict);
    format!(
        "PERF {} measured={:.3}{} target={} ceiling={} {}",
        b.name,
        measured,
        b.unit,
        b.target,
        b.ceiling,
        verdict_word(&verdict)
    )
}

fn verdict_word(v: &Verdict) -> &'static str {
    match v {
        Verdict::Pass => "PASS",
        Verdict::Warn => "WARN",
        Verdict::Fail => "FAIL",
    }
}

// --- trend JSONL ------------------------------------------------

/// True when `CASTELLUM_PERF_ADVISORY=1`: ceiling failures are reported but
/// never fail the process (the Linux `perf-trend` lane; the budgets are tied
/// to M-series hardware). Unset on the Mac `perf-gate` job, which is unchanged.
pub fn advisory() -> bool {
    std::env::var("CASTELLUM_PERF_ADVISORY").is_ok_and(|v| v == "1")
}

/// First number of a load-average string: Linux `/proc/loadavg`
/// (`0.52 0.58 ...`) or macOS `sysctl vm.loadavg` (`{ 2.10 3.00 4.00 }`).
pub fn parse_load1(s: &str) -> Option<f64> {
    s.split_whitespace().find_map(|tok| tok.parse::<f64>().ok())
}

/// 1-minute load average, or `None` where it cannot be read.
fn load1() -> Option<f64> {
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        return parse_load1(&s);
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()?;
    parse_load1(&String::from_utf8_lossy(&out.stdout))
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix timestamp (Howard Hinnant's
/// civil-from-days; avoids pulling a date crate into an example).
pub fn rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// One JSONL row: name, measured, unit, target, ceiling, verdict, sha,
/// runner, load1, ts. Pure, so it is unit-tested; never contains mail data.
pub fn jsonl_row(
    b: &Budget,
    measured: f64,
    verdict: &Verdict,
    sha: &str,
    runner: &str,
    load1: Option<f64>,
    ts: &str,
) -> String {
    serde_json::json!({
        "name": b.name,
        "measured": measured,
        "unit": b.unit,
        "target": b.target,
        "ceiling": b.ceiling,
        "verdict": verdict_word(verdict),
        "sha": sha,
        "runner": runner,
        "load1": load1,
        "ts": ts,
    })
    .to_string()
}

pub fn append_line(path: &std::path::Path, line: &str) {
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{line}") {
                eprintln!("WARN: cannot write {}: {e}", path.display());
            }
        }
        Err(e) => eprintln!("WARN: cannot open {}: {e}", path.display()),
    }
}

/// Append one row to the file named by `CASTELLUM_PERF_JSONL`; a no-op when
/// the variable is unset (no behaviour change for the Mac gate).
/// `sha` <- `GITHUB_SHA` (else `unknown`), `runner` <-
/// `CASTELLUM_PERF_RUNNER` (else `local`).
pub fn record_row(b: &Budget, measured: f64, verdict: &Verdict) {
    let Some(path) = std::env::var_os("CASTELLUM_PERF_JSONL") else {
        return;
    };
    let sha = std::env::var("GITHUB_SHA").unwrap_or_else(|_| "unknown".into());
    let runner = std::env::var("CASTELLUM_PERF_RUNNER").unwrap_or_else(|_| "local".into());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = jsonl_row(
        b,
        measured,
        verdict,
        &sha,
        &runner,
        load1(),
        &rfc3339_utc(now),
    );
    append_line(std::path::Path::new(&path), &line);
}

/// Moved verbatim from `sync_perf_gate.rs`.
pub fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(rfc3339_utc(1_790_857_377), "2026-10-01T12:22:57Z");
    }

    #[test]
    fn jsonl_row_has_the_ten_fields() {
        let b = Budget {
            name: "list_first_page",
            unit: "ms",
            target: 25.0,
            ceiling: 60.0,
        };
        let row = jsonl_row(
            &b,
            31.5,
            &Verdict::Warn,
            "abc123",
            "ci-runner",
            Some(1.25),
            "2026-10-01T12:00:00Z",
        );
        assert!(!row.contains('\n'), "one object per line");
        let v: serde_json::Value = serde_json::from_str(&row).expect("valid JSON");
        assert_eq!(v.as_object().unwrap().len(), 10);
        assert_eq!(v["name"], "list_first_page");
        assert_eq!(v["measured"], 31.5);
        assert_eq!(v["unit"], "ms");
        assert_eq!(v["target"], 25.0);
        assert_eq!(v["ceiling"], 60.0);
        assert_eq!(v["verdict"], "WARN");
        assert_eq!(v["sha"], "abc123");
        assert_eq!(v["runner"], "ci-runner");
        assert_eq!(v["load1"], 1.25);
        assert_eq!(v["ts"], "2026-10-01T12:00:00Z");
    }

    #[test]
    fn jsonl_row_unknown_load_is_null_and_names_are_escaped() {
        let b = Budget {
            name: "we\"ird",
            unit: "x",
            target: 1.0,
            ceiling: 2.0,
        };
        let row = jsonl_row(&b, 0.5, &Verdict::Pass, "unknown", "local", None, "t");
        let v: serde_json::Value = serde_json::from_str(&row).unwrap();
        assert!(v["load1"].is_null());
        assert_eq!(v["name"], "we\"ird");
    }

    #[test]
    fn parse_load1_handles_linux_and_bsd_shapes() {
        assert_eq!(parse_load1("0.52 0.58 0.59 1/467 12345"), Some(0.52));
        assert_eq!(parse_load1("{ 2.10 3.00 4.00 }"), Some(2.10));
        assert_eq!(parse_load1(""), None);
    }

    #[test]
    fn record_row_is_a_no_op_without_the_env_var() {
        // The env var is never set in the test process.
        assert!(std::env::var_os("CASTELLUM_PERF_JSONL").is_none());
        let b = Budget {
            name: "n",
            unit: "ms",
            target: 1.0,
            ceiling: 2.0,
        };
        record_row(&b, 1.0, &Verdict::Pass);
    }

    #[test]
    fn append_line_appends_one_line_per_call() {
        let path =
            std::env::temp_dir().join(format!("castellum-perf-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        append_line(&path, "{\"a\":1}");
        append_line(&path, "{\"a\":2}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\"a\":1}\n{\"a\":2}\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn same_seed_same_corpus() {
        let account = &corpus_layout(1_000)[0];
        let mailbox = account.mailboxes[0];
        let mut rng_a = Rng::new(2266);
        let mut rng_b = Rng::new(2266);
        let a = synthetic_descriptors(&mut rng_a, account, &mailbox);
        let b = synthetic_descriptors(&mut rng_b, account, &mailbox);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.subject, y.subject);
            assert_eq!(
                x.from.as_ref().map(|f| &f.address),
                y.from.as_ref().map(|f| &f.address)
            );
            assert_eq!(x.sent_at, y.sent_at);
            assert_eq!(x.references, y.references);
        }
    }

    #[test]
    fn layout_sums_to_total() {
        let total_100k: i64 = corpus_layout(100_000)
            .iter()
            .flat_map(|a| a.mailboxes.iter())
            .map(|m| m.count)
            .sum();
        assert_eq!(total_100k, 100_000);

        let total_10k: i64 = corpus_layout(10_000)
            .iter()
            .flat_map(|a| a.mailboxes.iter())
            .map(|m| m.count)
            .sum();
        assert_eq!(total_10k, 10_000);

        // Proportions preserved exactly for a layout that divides evenly.
        let scaled = corpus_layout(10_000);
        assert_eq!(scaled[0].mailboxes[0].count, 4_500); // acct-a INBOX
        assert_eq!(scaled[0].mailboxes[1].count, 2_500); // acct-a Archive
        assert_eq!(scaled[0].mailboxes[2].count, 1_000); // acct-a Sent
        assert_eq!(scaled[1].mailboxes[0].count, 1_500); // acct-b INBOX
        assert_eq!(scaled[1].mailboxes[1].count, 500); // acct-b Archive
    }

    #[test]
    fn null_and_duplicate_sent_at_present() {
        let account = &corpus_layout(1_000)[0];
        let mailbox = account.mailboxes[0]; // count 450 at this scale
        let mut rng = Rng::new(2266);
        let descriptors = synthetic_descriptors(&mut rng, account, &mailbox);
        assert!(
            descriptors.iter().any(|d| d.sent_at.is_none()),
            "expected at least one NULL sent_at in a mailbox this size"
        );
        let mut seen_ts = std::collections::HashSet::new();
        let mut has_duplicate = false;
        for d in &descriptors {
            if let Some(ts) = d.sent_at {
                if !seen_ts.insert(ts) {
                    has_duplicate = true;
                }
            }
        }
        assert!(has_duplicate, "expected at least one duplicated timestamp");
    }

    #[test]
    fn single_inbox_matches_legacy_formula() {
        let mut provider = SyntheticProvider::single_inbox(20);
        provider.select("INBOX").expect("select INBOX");
        let descriptors = provider
            .fetch_new_descriptors(None)
            .expect("fetch_new_descriptors");
        let uid17 = descriptors.iter().find(|d| d.uid == 17).unwrap();
        assert!(uid17.is_flagged, "uid 17 must be flagged (17 % 17 == 0)");
        let uid3 = descriptors.iter().find(|d| d.uid == 3).unwrap();
        assert!(!uid3.is_seen, "uid 3 must be unread (3 % 3 == 0)");
        assert_eq!(uid3.subject.as_deref(), Some("Synthetic message 3"));
    }

    #[test]
    fn judge_boundaries() {
        let b = Budget {
            name: "x",
            unit: "ms",
            target: 10.0,
            ceiling: 40.0,
        };
        assert_eq!(judge(&b, 10.0), Verdict::Pass);
        assert_eq!(judge(&b, 25.0), Verdict::Warn);
        assert_eq!(judge(&b, 40.0), Verdict::Warn);
        assert_eq!(judge(&b, 40.001), Verdict::Fail);
    }

    #[test]
    fn multi_mailbox_provider_selects_and_lists() {
        let account = &corpus_layout(200)[0];
        let mut rng = Rng::new(2266);
        let mut provider = SyntheticProvider::new(account, &mut rng, true);
        let mailboxes = provider.list_mailboxes().expect("list_mailboxes");
        assert_eq!(mailboxes.len(), account.mailboxes.len());
        provider.select("Archive").expect("select Archive");
        let descriptors = provider
            .fetch_new_descriptors(None)
            .expect("fetch_new_descriptors");
        assert_eq!(descriptors.len(), account.mailboxes[1].count as usize);
        assert!(provider.qresync_available());
        assert_eq!(provider.last_highest_modseq(), Some(MOCK_HIGHEST_MODSEQ));
        let outcome = provider
            .select_qresync("Archive", 1, MOCK_HIGHEST_MODSEQ)
            .expect("select_qresync");
        assert!(outcome.changed.is_empty());
    }
}
