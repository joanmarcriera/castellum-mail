// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - IMAP IDLE end-to-end test against a local GreenMail server
// (S31). #[ignore] by default, same convention as every other *_greenmail.rs
// file (see imap_greenmail.rs's header for the run command). GreenMail
// supports IDLE, so this is the one automated proof that a message delivered
// by someone/something else is noticed and imported WITHOUT a manual sync
// call — the scripted-TCP wire tests in castellum-provider-imap/tests/
// (session_wire.rs, idle_wire.rs) already cover the protocol shape in
// isolation; this proves the whole supervised loop end to end.

mod common;

use castellum_core::open_profile;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-greenmail-idle-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Sum of every mailbox's `total` from `Profile::mailbox_counts_json` — the
/// account here syncs INBOX only, so this is simply "how many messages does
/// the local DB hold for this account right now", read with zero network I/O
/// (never triggers a sync itself).
fn local_message_total(profile: &castellum_core::Profile, account_id: &str) -> i64 {
    let json = profile
        .mailbox_counts_json(account_id)
        .expect("mailbox_counts_json");
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    v.as_array()
        .map(|rows| rows.iter().filter_map(|r| r["total"].as_i64()).sum())
        .unwrap_or(0)
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn idle_notices_a_delivered_message_without_a_manual_sync() {
    let email = common::provision("idle-e2e");
    let account_id = "gm-idle";
    let config = common::sync_config_json(account_id, &email);

    let dir = temp_profile_dir("idle");
    let profile = Arc::new(open_profile(&dir).expect("open profile"));

    // One ordinary manual sync first — every real account gets this on add;
    // it also establishes the local baseline count (the provisioning
    // bootstrap message) before IDLE takes over.
    profile
        .sync_imap_json(&config)
        .expect("initial sync should succeed");
    let baseline = local_message_total(&profile, account_id);

    profile.start_idle(&config).expect("start_idle");

    // The thread connects, negotiates IDLE and starts waiting — bounded poll
    // rather than a fixed sleep guess.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !profile.is_idle_running(account_id) {
        assert!(
            Instant::now() < deadline,
            "IDLE thread never reported running (GreenMail may not advertise IDLE)"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // `is_idle_running` now only flips true once `idle_start` has actually
    // succeeded (S31 review fix: it used to flip the instant the
    // supervisor's map entry was inserted, before the thread had connected,
    // SELECTed INBOX or sent IDLE at all). Kept this settle margin anyway —
    // "IDLE started" and "the server has actually begun accepting pushes"
    // are not provably the same instant, and the original race (a message
    // landing before the wire IDLE, folded into the baseline EXISTS count
    // instead of arriving as a live push) is cheap to guard against. On
    // localhost, connect+LOGIN+CAPABILITY+SELECT+IDLE is a handful of RTTs
    // and finishes in well under a second; this margin is generous, not
    // tight.
    std::thread::sleep(Duration::from_secs(2));

    // Deliver a new message the way a real sender would — NOT through the
    // account's own IMAP session, so this is indistinguishable from mail
    // genuinely arriving while Castellum is idling.
    common::smtp_deliver(
        "sender@local.test",
        &email,
        "Subject: castellum idle e2e probe\r\n\r\nIDLE should notice this without a manual sync.\r\n",
    );

    // Bounded poll for the IDLE loop to wake, resync, and persist the new
    // message locally — well over one idle_wait tick (20s in idle.rs) would
    // be needed only if GreenMail delayed the push; in practice it pushes
    // EXISTS immediately on delivery, so this is generous, not tight.
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let total = local_message_total(&profile, account_id);
        if total > baseline {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "IDLE never noticed the delivered message within the deadline \
             (baseline={baseline}, still={total})"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    // Clean shutdown: stop_idle blocks until the thread has fully exited.
    profile.stop_idle(account_id);
    assert!(
        !profile.is_idle_running(account_id),
        "IDLE thread must be gone after stop_idle returns"
    );
}
