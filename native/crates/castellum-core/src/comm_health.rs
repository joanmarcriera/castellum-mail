// Castellum - explainable communication-health JSON wrappers.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Thin serialization layer over castellum-db's `comm_health` (computed from
// existing tables, no migration). Output for both calls is one JSON object:
//   {subject_kind, subject_id, as_of,
//    signals: [{key, value|null, unit, explanation, evidence_kind,
//               evidence_ids: [..]}]}
// or the string `null` for an unknown contact/thread. Evidence is ids only
// (message ids; work-item ids for `waiting_since_age`) -- never bodies.
// `*_at` variants take the clock so tests are deterministic.

use crate::{CoreError, Profile};

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn to_json(h: Option<castellum_db::CommunicationHealth>) -> Result<String, CoreError> {
    serde_json::to_string(&h).map_err(|_| CoreError::Io)
}

impl Profile {
    /// Communication-health signals for one contact now.
    pub fn contact_health_json(&self, contact_id: i64) -> Result<String, CoreError> {
        self.contact_health_json_at(contact_id, now_epoch())
    }

    /// [`Profile::contact_health_json`] with an injected clock (epoch secs).
    pub fn contact_health_json_at(&self, contact_id: i64, now: i64) -> Result<String, CoreError> {
        to_json(
            self.db
                .contact_health_at(contact_id, now)
                .map_err(|e| CoreError::Db(e.to_string()))?,
        )
    }

    /// Communication-health signals for one thread now.
    pub fn thread_health_json(&self, thread_id: &str) -> Result<String, CoreError> {
        self.thread_health_json_at(thread_id, now_epoch())
    }

    /// [`Profile::thread_health_json`] with an injected clock (epoch secs).
    pub fn thread_health_json_at(&self, thread_id: &str, now: i64) -> Result<String, CoreError> {
        to_json(
            self.db
                .thread_health_at(thread_id, now)
                .map_err(|e| CoreError::Db(e.to_string()))?,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::open_profile;
    use castellum_models::{Account, Address, Mailbox, NewMessage};

    const D: i64 = 86_400;
    const T0: i64 = 1_800_000_000;

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-comm-health-core-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        open_profile(&dir).unwrap()
    }

    fn msg(
        uid: i64,
        from: &str,
        to: &str,
        at: i64,
        refs: Vec<String>,
        subject: &str,
    ) -> NewMessage {
        // Our own mail lives in Sent: outbound needs a sent-role mailbox.
        let mb = if from == "me@local.test" {
            "Sent"
        } else {
            "INBOX"
        };
        NewMessage {
            id: format!("acc:{mb}:{uid}"),
            account_id: "acc".into(),
            mailbox_id: format!("acc:{mb}"),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@t>")),
            subject: Some(subject.into()),
            from_name: None,
            from_address: Some(from.into()),
            sent_at: Some(at),
            snippet: Some("SECRET BODY SNIPPET".into()),
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: true,
            is_starred: false,
            addresses: vec![
                Address {
                    kind: "from".into(),
                    name: None,
                    address: from.into(),
                },
                Address {
                    kind: "to".into(),
                    name: None,
                    address: to.into(),
                },
            ],
            text_body: Some("SECRET BODY".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: refs,
        }
    }

    #[test]
    fn contact_and_thread_signals_are_explained_with_ids_only() {
        let p = profile("e2e");
        p.db.upsert_account(&Account {
            id: "acc".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        for (name, role) in [("INBOX", "inbox"), ("Sent", "sent")] {
            p.db.upsert_mailbox(&Mailbox {
                id: format!("acc:{name}"),
                account_id: "acc".into(),
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
        // Thread A: bob writes, I reply after 2h, bob writes again (open).
        // Thread B: bob writes, never answered.
        let a = "bob@x.test";
        let me = "me@local.test";
        p.db.upsert_message(&msg(1, a, me, T0, vec![], "A"))
            .unwrap();
        p.db.upsert_message(&msg(2, me, a, T0 + 7_200, vec!["<1@t>".into()], "A"))
            .unwrap();
        p.db.upsert_message(&msg(
            3,
            a,
            me,
            T0 + 2 * D,
            vec!["<1@t>".into(), "<2@t>".into()],
            "A",
        ))
        .unwrap();
        p.db.upsert_message(&msg(4, a, me, T0 + 3 * D, vec![], "B"))
            .unwrap();
        p.db.rebuild_threads("acc").unwrap();
        let bob = p.db.create_contact(a, Some("Bob")).unwrap();
        let tid =
            p.db.list_thread_summaries("acc", 10, 0)
                .unwrap()
                .into_iter()
                .find(|t| t.subject.as_deref() == Some("A"))
                .unwrap()
                .id;

        // A waiting work item linked to thread A and to bob.
        let w: serde_json::Value = serde_json::from_str(
            &p.upsert_work_item_json_at(r#"{"title":"Chase","state":"waiting"}"#, T0 + D)
                .unwrap(),
        )
        .unwrap();
        let wid = w["id"].as_str().unwrap();
        p.link_work_item_json(wid, "thread", &tid).unwrap();
        p.link_work_item_json(wid, "contact", &bob.id.to_string())
            .unwrap();

        let now = T0 + 10 * D;
        let raw = p.contact_health_json_at(bob.id, now).unwrap();
        // Ids only: no body/snippet text anywhere in the payload.
        assert!(!raw.contains("SECRET"), "{raw}");
        let h: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(h["subject_kind"], "contact");
        assert_eq!(h["as_of"], now);
        let sigs = h["signals"].as_array().unwrap();
        let get = |k: &str| sigs.iter().find(|s| s["key"] == k).unwrap();
        // i1 answered after 2h; i3 and i4 unanswered (i3's thread has no later
        // outbound; i4 is threadless-of-reply).
        assert_eq!(get("reply_latency")["value"], 7_200.0);
        assert_eq!(get("unanswered_inbound")["value"], 2.0);
        assert_eq!(
            get("unanswered_inbound")["evidence_ids"],
            serde_json::json!(["acc:INBOX:4", "acc:INBOX:3"])
        );
        // 4 events at 0, 2h, 2d, 3d: gaps 2h, ~46h, 1d -> median 1d;
        // silence since day 3 to day 10 = 7d -> 7.0x.
        assert_eq!(get("cadence_gap")["value"], 7.0);
        // waiting since T0 + 1d, now T0 + 10d -> 9 days.
        assert_eq!(get("waiting_since_age")["value"], (9 * D) as f64);
        assert_eq!(
            get("waiting_since_age")["evidence_ids"],
            serde_json::json!([wid])
        );
        assert!(sigs
            .iter()
            .all(|s| !s["explanation"].as_str().unwrap().is_empty()));

        let t: serde_json::Value =
            serde_json::from_str(&p.thread_health_json_at(&tid, now).unwrap()).unwrap();
        assert_eq!(t["subject_kind"], "thread");
        let tg = |k: &str| {
            t["signals"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["key"] == k)
                .unwrap()
                .clone()
        };
        assert_eq!(tg("reply_latency")["value"], 7_200.0);
        assert_eq!(tg("unanswered_inbound")["value"], 1.0); // message 3 only
        assert_eq!(tg("waiting_since_age")["value"], (9 * D) as f64);

        assert_eq!(p.contact_health_json_at(99_999, now).unwrap(), "null");
        assert_eq!(p.thread_health_json_at("nope", now).unwrap(), "null");
    }
}
