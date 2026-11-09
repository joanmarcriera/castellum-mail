// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end account-setup preflight (S62 "Test Connection") against a local
// GreenMail server: real IMAP LOGIN/LIST and real SMTP AUTH, and proof that a
// test — passing or failing — persists nothing in the profile.
//
// #[ignore] by default (needs the scripts/local-imap GreenMail container):
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test connection_test_greenmail -- --ignored

mod common;

use castellum_core::open_profile;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-conntest-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn request(email: &str, password: &str, smtp_password: &str) -> String {
    serde_json::json!({
        "host": common::host(),
        "port": common::IMAP_PORT,
        "username": email,
        "password": password,
        "use_tls": false,
        "smtp": {
            "host": common::host(),
            "port": common::SMTP_PORT,
            "username": email,
            "password": smtp_password,
            "use_tls": false
        }
    })
    .to_string()
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn valid_credentials_pass_both_legs_and_list_inbox_without_persisting() {
    let email = common::provision("conntest-ok");
    let dir = temp_profile_dir("ok");
    let profile = open_profile(&dir).expect("open profile");

    let json = profile
        .test_connection_json(&request(&email, &email, &email))
        .expect("well-formed request");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["imap"]["ok"], true, "{json}");
    assert_eq!(v["smtp"]["ok"], true, "{json}");
    let inbox = v["mailboxes"]
        .as_array()
        .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
        .unwrap_or_else(|| panic!("INBOX missing from {json}"));
    assert_eq!(inbox["sync_by_default"], true);

    // Nothing was saved: no account row appears.
    let accounts = profile.list_accounts_json().expect("list accounts");
    assert_eq!(
        accounts, "[]",
        "a connection test must not create an account"
    );
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn a_wrong_imap_password_is_a_redacted_auth_failure() {
    let email = common::provision("conntest-badpw");
    let dir = temp_profile_dir("badpw");
    let profile = open_profile(&dir).expect("open profile");

    let json = profile
        .test_connection_json(&request(&email, "definitely-wrong-SENTINEL", &email))
        .expect("a connection failure is a report, not an error");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["imap"]["ok"], false, "{json}");
    assert_eq!(v["imap"]["error_kind"], "auth", "{json}");
    assert!(v["mailboxes"].as_array().unwrap().is_empty());
    assert!(!json.contains("SENTINEL"), "credential echoed: {json}");
    assert_eq!(profile.list_accounts_json().unwrap(), "[]");
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn an_unreachable_port_is_a_network_failure() {
    let dir = temp_profile_dir("unreachable");
    let profile = open_profile(&dir).expect("open profile");
    // Port 1 on the GreenMail host: nothing listens, connect is refused fast.
    let req = serde_json::json!({
        "host": common::host(), "port": 1, "username": "x", "password": "y",
        "use_tls": false
    })
    .to_string();
    let json = profile.test_connection_json(&req).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["imap"]["error_kind"], "network", "{json}");
    assert!(v["smtp"].is_null(), "no smtp leg was requested: {json}");
}
