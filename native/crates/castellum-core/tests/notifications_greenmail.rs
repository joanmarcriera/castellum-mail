// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S59 new-mail notification outbox against real GreenMail: a first
// sync announces nothing, later arrivals are announced exactly once, the VIP
// flag travels with the notice, and a muted thread's reply is not announced.
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test notifications_greenmail -- --ignored --nocapture

mod common;

use castellum_core::open_profile;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-notify-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The current time as an RFC 5322 `Date:` value (UTC). The outbox only
/// announces mail sent within the last 24 hours, so a fixed date would never
/// be announced.
fn rfc2822_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} +0000",
        WEEKDAYS[days.rem_euclid(7) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn take(profile: &castellum_core::Profile) -> Vec<serde_json::Value> {
    let v: serde_json::Value =
        serde_json::from_str(&profile.take_pending_notifications_json(50).expect("take"))
            .expect("json");
    v.as_array().expect("array").clone()
}

fn deliver(from: &str, email: &str, subject: &str, id: &str, extra_headers: &str) {
    common::smtp_deliver(
        from,
        email,
        &format!(
            "From: {from}\r\nTo: {email}\r\nSubject: {subject}\r\nDate: {}\r\n\
             Message-ID: <{id}@local.test>\r\n{extra_headers}\r\nNotification e2e body.\r\n",
            rfc2822_now()
        ),
    );
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn first_sync_announces_nothing_then_new_mail_is_announced() {
    let email = common::provision("notify-basic");
    let tag = common::run_tag("nb");
    let dir = temp_profile_dir("basic");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);

    // Provisioning delivered seed mail; the first sync must not announce it.
    profile.sync_imap_json(&config).expect("first sync");
    assert!(take(&profile).is_empty(), "first sync announces nothing");

    let subj = format!("castl-notify-{tag}");
    deliver(
        "someone@local.test",
        &email,
        &subj,
        &format!("nb-{tag}"),
        "",
    );
    profile.sync_imap_json(&config).expect("resync");
    let got = take(&profile);
    assert_eq!(got.len(), 1, "exactly the new message: {got:?}");
    assert_eq!(got[0]["subject"].as_str(), Some(subj.as_str()));
    assert_eq!(got[0]["from_vip"], false);
    assert!(take(&profile).is_empty(), "delivery is at-most-once");

    // A further resync with nothing new stays quiet.
    profile.sync_imap_json(&config).expect("idle resync");
    assert!(take(&profile).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn vip_flag_travels_with_the_notice() {
    let email = common::provision("notify-vip");
    let tag = common::run_tag("nv");
    let dir = temp_profile_dir("vip");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("first sync");
    assert!(take(&profile).is_empty());

    profile
        .set_vip_json("Boss@Local.Test", Some("The Boss"), true)
        .expect("set vip");
    let subj = format!("castl-vip-{tag}");
    deliver("boss@local.test", &email, &subj, &format!("nv-{tag}"), "");
    profile.sync_imap_json(&config).expect("resync");
    let got = take(&profile);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["from_vip"], true, "{got:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn muted_thread_reply_is_not_announced() {
    let email = common::provision("notify-mute");
    let tag = common::run_tag("nm");
    let subj = format!("castl-notify-mute-{tag}");
    // The original arrives before the first sync, so it is seeded silently.
    deliver("probe@local.test", &email, &subj, &format!("nm-{tag}"), "");

    let dir = temp_profile_dir("mute");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("first sync");
    assert!(take(&profile).is_empty());

    let list: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    let id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subj.as_str()))
        .expect("probe present after sync")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    profile.set_thread_muted(&id, true).expect("mute");

    deliver(
        "replier@local.test",
        &email,
        &format!("Re: {subj}"),
        &format!("nm-reply-{tag}"),
        &format!("In-Reply-To: <nm-{tag}@local.test>\r\nReferences: <nm-{tag}@local.test>\r\n"),
    );
    profile.sync_imap_json(&config).expect("resync after reply");
    let got = take(&profile);
    assert!(got.is_empty(), "a muted thread's reply is silent: {got:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
