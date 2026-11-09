// Castellum - contact network graph JSON API (slice 8c).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Computes a top-N-by-weighted-degree subgraph from the `contact_edges` table
// and serialises it as `{"nodes":[{id,label,message_count}],"edges":[{a,b,weight}]}`.
// Hidden contacts and the user's own address (is_self) are always excluded.
// An optional project_id filter restricts nodes to the project's linked contacts;
// edges are also trimmed so both endpoints must be in the selected node set.

use crate::{CoreError, Profile};

// ---------------------------------------------------------------------------
// Public params struct (avoids >7-parameter functions)
// ---------------------------------------------------------------------------

pub struct GraphParams<'a> {
    /// When `Some`, restrict nodes and edges to contacts linked to this project.
    pub project_id: Option<&'a str>,
    /// Hard-clamped to `[1, 200]` before any query.
    pub max_nodes: i64,
}

// ---------------------------------------------------------------------------
// Profile method
// ---------------------------------------------------------------------------

impl Profile {
    /// Build a top-N contact subgraph and return it as JSON.
    pub fn contact_graph_json(&self, params: GraphParams<'_>) -> Result<String, CoreError> {
        let max_nodes = params.max_nodes.clamp(1, 200);
        let value = self
            .db
            .contact_graph(params.project_id, max_nodes)
            .map_err(|e| CoreError::Db(e.to_string()))?;
        serde_json::to_string(&value).map_err(|_| CoreError::Io)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::open_profile;
    use castellum_models::{Account, Address, Mailbox, NewMessage};

    const NOW: i64 = 1_800_000_000;

    fn profile(tag: &str) -> crate::Profile {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-graph-core-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    fn seed_account(p: &crate::Profile) {
        p.db.upsert_account(&Account {
            id: "acc1".into(),
            email: "me@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        p.db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn msg(uid: i64, participants: Vec<(&str, &str, Option<&str>)>) -> NewMessage {
        let from = participants.iter().find(|(k, _, _)| *k == "from");
        NewMessage {
            id: format!("acc1:INBOX:{uid}"),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@t.test>")),
            subject: Some(format!("S{uid}")),
            from_name: from.and_then(|(_, _, n)| n.map(str::to_owned)),
            from_address: from.map(|(_, e, _)| e.to_string()),
            sent_at: Some(NOW),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: participants
                .iter()
                .map(|(k, e, n)| Address {
                    kind: k.to_string(),
                    name: n.map(str::to_owned),
                    address: e.to_string(),
                })
                .collect(),
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        }
    }

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn empty_graph_ok() {
        let p = profile("empty");
        seed_account(&p);
        p.db.refresh_contact_aggregates(NOW).unwrap();

        let json = p
            .contact_graph_json(super::GraphParams {
                project_id: None,
                max_nodes: 100,
            })
            .unwrap();
        let v = parse(&json);
        assert_eq!(v["nodes"].as_array().unwrap().len(), 0);
        assert_eq!(v["edges"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn graph_caps_at_max_nodes_by_weighted_degree() {
        let p = profile("caps");
        seed_account(&p);
        // alice <-> bob (edge weight 1.0 each, degree 1.0)
        // carol <-> dave (edge weight 1.0 each, degree 1.0)
        // alice <-> carol (extra edge, gives alice + carol degree 2.0)
        p.db.upsert_message(&msg(
            1,
            vec![
                ("from", "alice@t.test", Some("Alice")),
                ("to", "bob@t.test", Some("Bob")),
            ],
        ))
        .unwrap();
        p.db.upsert_message(&msg(
            2,
            vec![
                ("from", "alice@t.test", Some("Alice")),
                ("to", "carol@t.test", Some("Carol")),
            ],
        ))
        .unwrap();
        p.db.upsert_message(&msg(
            3,
            vec![
                ("from", "carol@t.test", Some("Carol")),
                ("to", "dave@t.test", Some("Dave")),
            ],
        ))
        .unwrap();
        p.db.refresh_contact_aggregates(NOW).unwrap();

        let json = p
            .contact_graph_json(super::GraphParams {
                project_id: None,
                max_nodes: 2,
            })
            .unwrap();
        let v = parse(&json);
        let nodes = v["nodes"].as_array().unwrap();
        // Only 2 nodes despite 4 contacts: alice and carol have degree 2.0 each.
        assert_eq!(nodes.len(), 2);
        let labels: Vec<&str> = nodes.iter().map(|n| n["label"].as_str().unwrap()).collect();
        assert!(
            labels.contains(&"Alice"),
            "alice must be in top-2 by degree: {labels:?}"
        );
        assert!(
            labels.contains(&"Carol"),
            "carol must be in top-2 by degree: {labels:?}"
        );
    }

    #[test]
    fn hidden_and_self_excluded() {
        let p = profile("hidself");
        seed_account(&p);
        p.db.upsert_message(&msg(
            1,
            vec![
                ("from", "alice@t.test", Some("Alice")),
                ("to", "bob@t.test", Some("Bob")),
                ("to", "me@local.test", None),
            ],
        ))
        .unwrap();
        p.db.refresh_contact_aggregates(NOW).unwrap();
        // Hide alice.
        let contacts = p.db.list_contacts(None, true, 100).unwrap();
        let alice = contacts.iter().find(|c| c.email == "alice@t.test").unwrap();
        p.db.update_contact_user_fields(alice.id, None, String::new(), true)
            .unwrap();

        let json = p
            .contact_graph_json(super::GraphParams {
                project_id: None,
                max_nodes: 100,
            })
            .unwrap();
        let v = parse(&json);
        let nodes = v["nodes"].as_array().unwrap();
        let labels: Vec<&str> = nodes.iter().map(|n| n["label"].as_str().unwrap()).collect();
        assert!(
            !labels.contains(&"Alice"),
            "hidden alice must be excluded: {labels:?}"
        );
        assert!(
            !labels.contains(&"me@local.test"),
            "self must be excluded: {labels:?}"
        );
    }

    #[test]
    fn labels_prefer_display_name() {
        let p = profile("labels");
        seed_account(&p);
        // alice <-> bob share a message → they get an edge → both have degree > 0
        // so both show up in the global graph.
        p.db.upsert_message(&msg(
            1,
            vec![
                ("from", "alice@t.test", Some("Alice Header")),
                ("to", "bob@t.test", Some("Bob")),
            ],
        ))
        .unwrap();
        p.db.refresh_contact_aggregates(NOW).unwrap();
        let contacts = p.db.list_contacts(None, true, 100).unwrap();
        let alice = contacts.iter().find(|c| c.email == "alice@t.test").unwrap();
        // Set a user display_name override — it should take priority in the label.
        p.db.update_contact_user_fields(
            alice.id,
            Some("Alice Override".into()),
            String::new(),
            false,
        )
        .unwrap();

        let json = p
            .contact_graph_json(super::GraphParams {
                project_id: None,
                max_nodes: 100,
            })
            .unwrap();
        let v = parse(&json);
        let nodes = v["nodes"].as_array().unwrap();
        let labels: Vec<&str> = nodes.iter().map(|n| n["label"].as_str().unwrap()).collect();
        assert!(
            labels.contains(&"Alice Override"),
            "display_name must win over derived_name: {labels:?}"
        );
    }

    #[test]
    fn project_filter_requires_both_endpoints() {
        let p = profile("projfilt");
        seed_account(&p);
        // alice <-> bob share a message.
        p.db.upsert_message(&msg(
            1,
            vec![
                ("from", "alice@t.test", Some("Alice")),
                ("to", "bob@t.test", Some("Bob")),
            ],
        ))
        .unwrap();
        p.db.refresh_contact_aggregates(NOW).unwrap();

        // Create a project.
        let proj =
            p.db.upsert_project(castellum_db::ProjectUpsert {
                id: None,
                name: "TestProj",
                notes: "",
                status: "active",
                color: None,
                position: 0,
                archived: None,
            })
            .unwrap();

        // Link only alice to the project (not bob).
        let contacts = p.db.list_contacts(None, true, 100).unwrap();
        let alice = contacts.iter().find(|c| c.email == "alice@t.test").unwrap();
        p.db.set_contact_link(&proj.id, alice.id, true).unwrap();

        let json = p
            .contact_graph_json(super::GraphParams {
                project_id: Some(&proj.id),
                max_nodes: 100,
            })
            .unwrap();
        let v = parse(&json);
        let nodes = v["nodes"].as_array().unwrap();
        let edges = v["edges"].as_array().unwrap();

        // Only alice is in the project.
        assert_eq!(nodes.len(), 1, "only project member returned");
        let labels: Vec<&str> = nodes.iter().map(|n| n["label"].as_str().unwrap()).collect();
        assert!(labels.contains(&"Alice"));
        // No edges because bob is not in the project.
        assert_eq!(edges.len(), 0, "edge excluded: bob not in project");
    }

    /// C2 regression: a hidden project member must not (a) appear as a node,
    /// (b) leave dangling edges referencing an absent node, or (c) consume a
    /// top-N slot and displace a visible member.
    #[test]
    fn project_filter_excludes_hidden_from_ranking_and_edges() {
        let p = profile("projhidden");
        seed_account(&p);
        // eve is the busiest contact: edges to both alice and bob (degree 2+),
        // plus one alice <-> bob edge. If hidden eve were still pooled she
        // would rank #1 in top-2 and displace a visible member, and her edges
        // would dangle.
        p.db.upsert_message(&msg(
            1,
            vec![
                ("from", "eve@t.test", Some("Eve")),
                ("to", "alice@t.test", Some("Alice")),
            ],
        ))
        .unwrap();
        p.db.upsert_message(&msg(
            2,
            vec![
                ("from", "eve@t.test", Some("Eve")),
                ("to", "bob@t.test", Some("Bob")),
            ],
        ))
        .unwrap();
        p.db.upsert_message(&msg(
            3,
            vec![
                ("from", "alice@t.test", Some("Alice")),
                ("to", "bob@t.test", Some("Bob")),
            ],
        ))
        .unwrap();
        p.db.refresh_contact_aggregates(NOW).unwrap();

        let proj =
            p.db.upsert_project(castellum_db::ProjectUpsert {
                id: None,
                name: "HiddenProj",
                notes: "",
                status: "active",
                color: None,
                position: 0,
                archived: None,
            })
            .unwrap();

        // Link all three to the project, then hide eve.
        let contacts = p.db.list_contacts(None, true, 100).unwrap();
        let by_email = |e: &str| contacts.iter().find(|c| c.email == e).unwrap();
        let (alice, bob, eve) = (
            by_email("alice@t.test"),
            by_email("bob@t.test"),
            by_email("eve@t.test"),
        );
        for c in [alice, bob, eve] {
            p.db.set_contact_link(&proj.id, c.id, true).unwrap();
        }
        p.db.update_contact_user_fields(eve.id, None, String::new(), true)
            .unwrap();

        // max_nodes = 2: exactly enough for the two visible members — a pooled
        // hidden eve (pre-fix behaviour) would have stolen one of these slots.
        let json = p
            .contact_graph_json(super::GraphParams {
                project_id: Some(&proj.id),
                max_nodes: 2,
            })
            .unwrap();
        let v = parse(&json);
        let nodes = v["nodes"].as_array().unwrap();
        let edges = v["edges"].as_array().unwrap();

        let labels: Vec<&str> = nodes.iter().map(|n| n["label"].as_str().unwrap()).collect();
        assert_eq!(
            nodes.len(),
            2,
            "hidden eve must not consume a top-N slot: {labels:?}"
        );
        assert!(labels.contains(&"Alice"), "alice visible: {labels:?}");
        assert!(labels.contains(&"Bob"), "bob visible: {labels:?}");
        assert!(!labels.contains(&"Eve"), "hidden eve excluded: {labels:?}");

        // Every edge endpoint must reference an emitted node (no dangling edges).
        let node_ids: std::collections::HashSet<i64> =
            nodes.iter().map(|n| n["id"].as_i64().unwrap()).collect();
        assert_eq!(edges.len(), 1, "only the alice<->bob edge remains");
        for e in edges {
            assert!(node_ids.contains(&e["a"].as_i64().unwrap()), "dangling a");
            assert!(node_ids.contains(&e["b"].as_i64().unwrap()), "dangling b");
        }
    }
}
