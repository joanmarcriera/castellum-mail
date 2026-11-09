// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// F3: the thrown error of a FIRST-EVER sync (no accounts row yet, so no
// local-diagnostics row for the app to read the kind from) must itself carry
// the classified kind as a leading "<kind>: " token. Before the fix a wrong
// password on Add account surfaced as "Can't reach ... the connection failed"
// because the app fell back to "network".
//
// #[ignore] by default (needs the scripts/local-imap GreenMail container):
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test first_connect_error_kind_greenmail -- --ignored

mod common;

use castellum_core::open_profile;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-f3-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn config(account_id: &str, email: &str, password: &str, port: u16) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(&common::sync_config_json(account_id, email)).unwrap();
    v["password"] = password.into();
    v["port"] = port.into();
    v.to_string()
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn first_sync_with_a_wrong_password_is_classified_auth() {
    let email = common::provision("f3-wrongpw");
    let profile = open_profile(&temp_profile_dir("auth")).expect("open profile");
    let err = profile
        .sync_imap_json(&config(
            "f3-auth",
            &email,
            "definitely-wrong",
            common::IMAP_PORT,
        ))
        .expect_err("a wrong password must fail the sync");
    let text = format!("{err:?}");
    assert!(
        text.contains("auth: "),
        "expected leading auth kind: {text}"
    );
    assert!(
        !text.contains("network"),
        "must not read as network: {text}"
    );
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn first_sync_to_a_closed_port_is_classified_network() {
    let email = common::provision("f3-closed");
    let profile = open_profile(&temp_profile_dir("net")).expect("open profile");
    let err = profile
        .sync_imap_json(&config("f3-net", &email, &email, 1))
        .expect_err("a closed port must fail the sync");
    let text = format!("{err:?}");
    assert!(
        text.contains("network: "),
        "expected leading network kind: {text}"
    );
}
