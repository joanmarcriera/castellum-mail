// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end projects test against a local GreenMail server.
//
// #[ignore] by default: requires the GreenMail container from scripts/local-imap.
// The test provisions its own account (parallel-safe, no seeding needed).
// Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test projects_greenmail -- --ignored --nocapture
//
// What it covers:
//   1. Sync INBOX from GreenMail (appending its own multi-recipient message).
//   2. Refresh contact aggregates so `contacts` is populated.
//   3. Create a project and link the synced message with auto_contacts=true.
//   4. Assert the hydrated ProjectDetail contains the linked email header.
//   5. Assert alice (non-self, non-hidden sender) is auto-linked as origin='auto'.
//   6. Confirm the auto-link → origin changes to 'manual'.
//   7. Unlink the message, assert emails list is empty.
//   8. projects_for_message returns empty after unlink.
//   9. Move the project to 'done', assert status in listing.
//

mod common;

use castellum_core::open_profile;

const ALICE: &str = "alice@example.test";

/// Civil date helper (Howard Hinnant algorithm) — no date-crate dependency.
fn rfc2822_utc(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let secs = epoch % 86_400;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];
    format!("{weekday}, {d:02} {month_name} {year} {h:02}:{m:02}:{s:02} +0000")
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn projects_link_email_auto_contacts() {
    let self_addr = common::provision("projects");
    let tag = common::run_tag("projects");

    // ------------------------------------------------------------------
    // 1. Append a multi-recipient message to GreenMail INBOX.
    // ------------------------------------------------------------------
    let mut session = common::imap_session(&self_addr);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let rfc_msg_id = format!("<projects-e2e-{tag}@example.test>");
    let date = rfc2822_utc(now.saturating_sub(120));
    let raw = format!(
        "From: \"Alice Novak\" <{ALICE}>\r\n\
         To: {self_addr}\r\n\
         Subject: Projects E2E {tag}\r\n\
         Date: {date}\r\n\
         Message-ID: {rfc_msg_id}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         Hello, this is the projects e2e test message.\r\n"
    );
    session
        .append("INBOX", &[], raw.as_bytes())
        .expect("IMAP append");
    drop(session);

    // ------------------------------------------------------------------
    // 2. Open profile and sync account.
    // ------------------------------------------------------------------
    let dir = castellum_testutil::scratch_root().join(format!("castellum-projects-e2e-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = open_profile(&dir).expect("open profile");

    let config = common::sync_config_json("gm", &self_addr);
    let report = profile.sync_imap_json(&config).expect("sync_imap_json");
    println!("sync report: {report}");

    // ------------------------------------------------------------------
    // 3. Refresh contacts so auto_contacts can find alice.
    // ------------------------------------------------------------------
    let agg = profile
        .refresh_contacts_json()
        .expect("refresh_contacts_json");
    println!("contacts aggregate: {agg}");

    // ------------------------------------------------------------------
    // 4. Find the synced message by its unique tagged Subject.
    //
    // The list projection is MessageHeader (no rfc822_message_id field), so
    // match on the subject, which carries the per-process tag. The shared
    // GreenMail container accumulates messages from other test runs (sync
    // counts vary run-to-run), so no assertion is made on totals — only on
    // our own tagged message. A generous limit covers a well-used mailbox.
    // ------------------------------------------------------------------
    let msgs_json = profile
        .list_messages_json("gm", "gm:INBOX", 2000, 0)
        .expect("list_messages_json");
    let msgs: Vec<serde_json::Value> = serde_json::from_str(&msgs_json).unwrap();
    println!("messages count: {}", msgs.len());
    let wanted_subject = format!("Projects E2E {tag}");
    let our_msg = msgs
        .iter()
        .find(|m| m["subject"].as_str().unwrap_or("") == wanted_subject)
        .expect("seeded message should be in INBOX after sync");
    let message_id = our_msg["id"].as_str().unwrap().to_string();
    println!("found message_id={message_id}");

    // ------------------------------------------------------------------
    // 5. Create a project and link the message with auto_contacts=true.
    // ------------------------------------------------------------------
    let created_json = profile
        .upsert_project_json(r#"{"name":"E2E Project","status":"active"}"#)
        .expect("upsert_project_json");
    println!("created project: {created_json}");
    let created: serde_json::Value = serde_json::from_str(&created_json).unwrap();
    let project_id = created["id"].as_str().unwrap().to_string();
    assert!(!project_id.is_empty());

    profile
        .project_set_email_link(&project_id, &message_id, true, true)
        .expect("project_set_email_link");

    // ------------------------------------------------------------------
    // 6. Hydrate and assert: email present + alice auto-linked.
    // ------------------------------------------------------------------
    let detail_json = profile
        .get_project_json(&project_id)
        .expect("get_project_json");
    println!("hydrated detail: {detail_json}");
    assert!(
        detail_json.contains(&format!("\"id\":\"{message_id}\"")),
        "hydrated detail should contain the linked email id: {detail_json}"
    );
    assert!(
        detail_json.contains(ALICE),
        "alice should be auto-linked as a project contact: {detail_json}"
    );
    assert!(
        detail_json.contains("\"origin\":\"auto\""),
        "auto-linked contacts should have origin=auto: {detail_json}"
    );

    // ------------------------------------------------------------------
    // 7. Confirm alice → origin changes to 'manual'.
    // ------------------------------------------------------------------
    let detail: serde_json::Value = serde_json::from_str(&detail_json).unwrap();
    let alice_contact_id = detail["contacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["email"].as_str().unwrap_or("") == ALICE)
        .expect("alice must be in contacts")["contact_id"]
        .as_i64()
        .unwrap();

    profile
        .project_set_contact_link(&project_id, alice_contact_id, true)
        .expect("project_set_contact_link confirms auto→manual");

    let after_confirm = profile
        .get_project_json(&project_id)
        .expect("get_project_json after confirm");
    assert!(
        after_confirm.contains("\"origin\":\"manual\""),
        "after confirm, alice origin should be manual: {after_confirm}"
    );

    // ------------------------------------------------------------------
    // 8. projects_for_message: reverse lookup while linked.
    // ------------------------------------------------------------------
    let tags_json = profile
        .projects_for_message_json(&message_id)
        .expect("projects_for_message_json");
    assert!(
        tags_json.contains("\"name\":\"E2E Project\""),
        "reverse lookup should find our project: {tags_json}"
    );
    assert!(
        tags_json.contains(&format!("\"project_id\":\"{project_id}\"")),
        "reverse lookup should include project_id: {tags_json}"
    );

    // ------------------------------------------------------------------
    // 9. Unlink the message; emails list becomes empty.
    // ------------------------------------------------------------------
    profile
        .project_set_email_link(&project_id, &message_id, false, false)
        .expect("unlink email");

    let after_unlink = profile
        .get_project_json(&project_id)
        .expect("get_project_json after unlink");
    assert!(
        after_unlink.contains("\"emails\":[]"),
        "after unlink, emails should be empty: {after_unlink}"
    );

    // Reverse lookup now returns empty.
    let tags_after = profile
        .projects_for_message_json(&message_id)
        .expect("projects_for_message_json after unlink");
    assert_eq!(tags_after, "[]", "unlinked message should have no projects");

    // ------------------------------------------------------------------
    // 10. Move project to 'done'; assert status in listing.
    // ------------------------------------------------------------------
    profile
        .move_project(&project_id, "done", 0)
        .expect("move_project");

    let list_json = profile
        .list_projects_json(false)
        .expect("list_projects_json");
    assert!(
        list_json.contains("\"status\":\"done\""),
        "moved project should be in done column: {list_json}"
    );

    // ------------------------------------------------------------------
    // Clean-up (best-effort): delete the project.
    // ------------------------------------------------------------------
    let _ = profile.delete_project(&project_id);
    println!("projects e2e test passed");
}
