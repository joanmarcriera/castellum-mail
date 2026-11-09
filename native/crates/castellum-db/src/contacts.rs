// Castellum - contacts aggregation + people co-occurrence edges (slice 8a).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Aggregates a people layer from already-synced mail. `refresh_contact_aggregates`
// reads on a private read-only snapshot and writes in short chunks (S72, see
// its doc comment): contacts are aggregated in one SQL pass and upserted
// (derived columns only — user-owned fields are never touched), orphans are
// pruned, then the edge graph is recomputed and applied as a difference.
// The exponentially-decayed edge weights are computed in Rust from a single
// indexed scan: SQLite's `exp()` needs the optional math extension, which the
// bundled/host builds don't guarantee.

use crate::{Database, DbError};
use castellum_models::{AggregateReport, ContactRecord, EdgeRecord, SentRecipient};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Mailing-list guard: messages with more than this many (non-self, deduped)
/// participants contribute no edges — a 200-recipient newsletter says nothing
/// about who actually works with whom.
const MAX_EDGE_PARTICIPANTS: usize = 25;

/// Recency half-life driver for edge weights (see `decay`).
const DECAY_DAYS: f64 = 90.0;

/// Thread-only co-occurrence is worth a quarter of a shared message.
const THREAD_FACTOR: f64 = 0.25;

/// exp(-age_days / 90) from `sent_at` to `now`. Messages without a date decay
/// as if sent now (age 0) — better than dropping them entirely.
fn decay(now_epoch: i64, sent_at: Option<i64>) -> f64 {
    let age_secs = (now_epoch - sent_at.unwrap_or(now_epoch)).max(0);
    let age_days = age_secs as f64 / 86_400.0;
    (-age_days / DECAY_DAYS).exp()
}

/// Canonical (a < b) pair key for `contact_edges`.
fn pair(a: i64, b: i64) -> (i64, i64) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Accumulated state for one edge while the graph is being rebuilt.
#[derive(Default)]
struct EdgeAccum {
    weight: f64,
    /// Same-message co-occurrences only (thread bonuses don't count).
    message_count: i64,
    last_seen_at: Option<i64>,
}

impl EdgeAccum {
    fn touch(&mut self, sent_at: Option<i64>) {
        if let Some(s) = sent_at {
            self.last_seen_at = Some(self.last_seen_at.map_or(s, |cur| cur.max(s)));
        }
    }
}

impl Database {
    /// Recompute the contacts aggregate and rebuild the co-occurrence edge
    /// graph. Derived contact columns (derived_name, is_self,
    /// first/last_seen, counts) are recomputed; user-owned columns
    /// (display_name, notes, is_hidden, is_user_created) are preserved.
    ///
    /// Sync DOES hard-delete message rows (delete→Trash local removal,
    /// empty_trash, UIDVALIDITY purge), so contacts can lose every backing
    /// message: pure-derived ghosts are deleted; contacts carrying user data
    /// survive with their derived columns zeroed.
    ///
    /// S72: this used to be ONE transaction under the shared
    /// connection -- 0.4-1.7 s at 100k messages during which every sync
    /// upsert and every UI read (the progress poller, Settings > Accounts)
    /// queued behind it. Now each phase READS on its own read-only snapshot
    /// connection (`open_read_snapshot`: WAL readers never touch `conn`) and
    /// WRITES in short transactions of at most `WRITE_CHUNK` rows, releasing
    /// the connection between them. What that gives up is atomicity across
    /// the whole refresh: a reader can briefly see some contacts updated and
    /// others not, or new contacts before their edges -- all derived data,
    /// and the next refresh converges it. Every write re-checks its
    /// predicate (prune) or tolerates a vanished contact (edges), so a sync
    /// landing mid-refresh can never be clobbered with stale conclusions.
    /// Refreshes themselves are serialised (`lock_contacts_refresh`).
    pub fn refresh_contact_aggregates(&self, now_epoch: i64) -> Result<AggregateReport, DbError> {
        let _serial = self.lock_contacts_refresh();
        let started = Instant::now();
        let mut hold = HoldTimer::default();

        // 1. Contacts: aggregate on a snapshot, upsert in chunks -- only the
        // rows whose derived columns actually moved (a steady-state launch
        // refresh writes nothing; updated_at now means "derived data last
        // changed", not "last refreshed").
        let (rows, current) = self.read_snapshot(&mut hold, |c| {
            Ok((read_contact_aggregates(c)?, read_contact_derived(c)?))
        })?;
        let changed: Vec<&ContactAggRow> = rows
            .iter()
            .filter(|r| current.get(&r.email) != Some(&r.derived()))
            .collect();
        for chunk in changed.chunks(WRITE_CHUNK) {
            self.write_chunk(&mut hold, |tx| upsert_contact_rows(tx, chunk))?;
        }

        // 2. Prune: candidates from a snapshot that already sees step 1;
        // each write re-checks the full predicate.
        let orphans = self.read_snapshot(&mut hold, read_orphan_candidates)?;
        for chunk in orphans.chunks(WRITE_CHUNK) {
            self.write_chunk(&mut hold, |tx| prune_orphaned_contacts(tx, chunk))?;
        }

        // 3. Edges: compute the whole graph on a snapshot, then apply the
        // difference -- stale pairs deleted, new or changed pairs upserted.
        let (fresh, existing) = self.read_snapshot(&mut hold, |c| {
            Ok((compute_edges(c, now_epoch)?, read_edges(c)?))
        })?;
        let stale: Vec<(i64, i64)> = existing
            .keys()
            .filter(|k| !fresh.contains_key(k))
            .copied()
            .collect();
        for chunk in stale.chunks(WRITE_CHUNK) {
            self.write_chunk(&mut hold, |tx| delete_edges(tx, chunk))?;
        }
        let mut keys: Vec<(i64, i64)> = fresh
            .iter()
            .filter(|(k, acc)| existing.get(k).is_none_or(|old| edge_moved(old, acc)))
            .map(|(k, _)| *k)
            .collect();
        keys.sort_unstable(); // deterministic write order
        for chunk in keys.chunks(WRITE_CHUNK) {
            self.write_chunk(&mut hold, |tx| upsert_edges(tx, chunk, &fresh))?;
        }

        Ok(AggregateReport {
            contacts_upserted: rows.len() as i64,
            edges_rebuilt: fresh.len() as i64,
            elapsed_ms: started.elapsed().as_millis() as u64,
            max_lock_hold_ms: hold.max.as_millis() as u64,
        })
    }

    /// Run `read` inside one read transaction (one consistent snapshot) on a
    /// private read-only connection, so the shared `conn` stays free. Falls
    /// back to reading through `conn` (timed as a hold) when no snapshot
    /// connection can be opened.
    fn read_snapshot<T>(
        &self,
        hold: &mut HoldTimer,
        read: impl FnOnce(&Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        if let Some(mut snap) = self.open_read_snapshot() {
            let tx = snap.transaction()?;
            let out = read(&tx)?;
            tx.commit()?;
            return Ok(out);
        }
        let conn = self.lock_conn();
        let t = Instant::now();
        let out = read(&conn);
        drop(conn);
        hold.note(t);
        out
    }

    /// One short write transaction on the shared connection; the guard is
    /// dropped before returning so other callers get in between chunks.
    fn write_chunk<T>(
        &self,
        hold: &mut HoldTimer,
        write: impl FnOnce(&Transaction<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut conn = self.lock_conn();
        let t = Instant::now();
        let tx = conn.transaction()?;
        let out = write(&tx)?;
        tx.commit()?;
        drop(conn);
        hold.note(t);
        Ok(out)
    }

    /// List contacts, most recently seen first. `query` (if given) substring-
    /// matches email, derived name or display name, case-insensitively.
    pub fn list_contacts(
        &self,
        query: Option<&str>,
        include_hidden: bool,
        limit: i64,
    ) -> Result<Vec<ContactRecord>, DbError> {
        let conn = self.lock_conn();
        // DESC ordering puts NULL last_seen_at (user-created, never seen) last.
        let mut stmt = conn.prepare(
            "SELECT id, email, derived_name, display_name, notes, is_hidden, is_self,
                    is_user_created, first_seen_at, last_seen_at, inbound_count, outbound_count
             FROM contacts
             WHERE (is_hidden = 0 OR ?1)
               AND (?2 IS NULL
                    OR instr(lower(email), lower(?2)) > 0
                    OR instr(lower(COALESCE(derived_name, '')), lower(?2)) > 0
                    OR instr(lower(COALESCE(display_name, '')), lower(?2)) > 0)
             ORDER BY last_seen_at DESC, email
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![include_hidden, query, limit], row_to_contact)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Recipients (to/cc/bcc) of messages we sent or queued, newest first,
    /// distinct by lower-cased address and excluding our own account
    /// addresses. Read from the `outbox`/`sent` drafts, so an address we have
    /// just written to is suggestible immediately -- before the Sent copy
    /// syncs back into `messages` (or if Sent is never synced). Local only.
    pub fn list_sent_recipients(&self, limit: i64) -> Result<Vec<SentRecipient>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT lower(trim(json_extract(j.value, '$.email'))) AS email,
                    json_extract(j.value, '$.name') AS name,
                    CAST(strftime('%s', d.updated_at) AS INTEGER) AS ts
             FROM drafts d
             JOIN (SELECT drafts.id AS id, e.value AS value
                     FROM drafts, json_each(drafts.to_json) e
                   UNION ALL SELECT drafts.id, e.value
                     FROM drafts, json_each(drafts.cc_json) e
                   UNION ALL SELECT drafts.id, e.value
                     FROM drafts, json_each(drafts.bcc_json) e) j ON j.id = d.id
             WHERE d.status IN ('outbox', 'sent')
               AND email LIKE '%@%'
               AND email NOT IN (SELECT lower(email) FROM accounts)
               AND email NOT IN (SELECT email FROM contacts WHERE is_hidden = 1)
             ORDER BY ts DESC, d.id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })?;
        // Rows arrive newest first: the first sighting fixes last_sent_at, and
        // the first non-empty name seen is the most recently used one.
        let mut order: Vec<String> = Vec::new();
        let mut by_email: HashMap<String, SentRecipient> = HashMap::new();
        for row in rows {
            let (email, name, ts) = row?;
            let name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
            let entry = by_email.entry(email.clone()).or_insert_with(|| {
                order.push(email.clone());
                SentRecipient {
                    email,
                    name: None,
                    last_sent_at: ts,
                    sent_count: 0,
                }
            });
            entry.sent_count += 1;
            if entry.name.is_none() {
                entry.name = name;
            }
        }
        Ok(order
            .into_iter()
            .take(limit.max(0) as usize)
            .filter_map(|e| by_email.remove(&e))
            .collect())
    }

    /// One contact by id, or `None`.
    pub fn get_contact_detail(&self, id: i64) -> Result<Option<ContactRecord>, DbError> {
        let conn = self.lock_conn();
        let c = conn
            .query_row(
                "SELECT id, email, derived_name, display_name, notes, is_hidden, is_self,
                        is_user_created, first_seen_at, last_seen_at, inbound_count, outbound_count
                 FROM contacts WHERE id = ?1",
                params![id],
                row_to_contact,
            )
            .optional()?;
        Ok(c)
    }

    /// Update the user-owned fields of a contact. Refresh never touches these.
    pub fn update_contact_user_fields(
        &self,
        id: i64,
        display_name: Option<String>,
        notes: String,
        is_hidden: bool,
    ) -> Result<(), DbError> {
        let conn = self.lock_conn();
        let n = conn.execute(
            "UPDATE contacts
             SET display_name = ?2, notes = ?3, is_hidden = ?4, updated_at = datetime('now')
             WHERE id = ?1",
            params![id, display_name, notes, is_hidden],
        )?;
        if n == 0 {
            return Err(DbError(format!("contact {id} not found")));
        }
        Ok(())
    }

    /// Create a contact by hand (`is_user_created = 1`). The name goes to
    /// `display_name` (it is a user override, not a header-derived pick). If a
    /// contact with that email already exists it is marked user-created and its
    /// display_name is set (when provided) instead of failing.
    pub fn create_contact(
        &self,
        email: &str,
        name: Option<&str>,
    ) -> Result<ContactRecord, DbError> {
        let normalized = email.trim().to_lowercase();
        if normalized.is_empty() || !normalized.contains('@') {
            return Err(DbError(format!("invalid contact email: {email:?}")));
        }
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO contacts (email, display_name, is_user_created)
             VALUES (?1, ?2, 1)
             ON CONFLICT(email) DO UPDATE SET
                 is_user_created = 1,
                 display_name = COALESCE(excluded.display_name, contacts.display_name),
                 updated_at = datetime('now')",
            params![normalized, name],
        )?;
        let c = conn.query_row(
            "SELECT id, email, derived_name, display_name, notes, is_hidden, is_self,
                    is_user_created, first_seen_at, last_seen_at, inbound_count, outbound_count
             FROM contacts WHERE email = ?1",
            params![normalized],
            row_to_contact,
        )?;
        Ok(c)
    }

    /// Edges with `weight >= min_weight`, heaviest first.
    pub fn list_edges(&self, min_weight: f64, limit: i64) -> Result<Vec<EdgeRecord>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT contact_a, contact_b, weight, message_count, last_seen_at
             FROM contact_edges WHERE weight >= ?1
             ORDER BY weight DESC, contact_a, contact_b
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![min_weight, limit], |r| {
            Ok(EdgeRecord {
                contact_a: r.get(0)?,
                contact_b: r.get(1)?,
                weight: r.get(2)?,
                message_count: r.get(3)?,
                last_seen_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Build the top-N contact subgraph as a JSON value ready for the FFI.
    ///
    /// Node selection: top `max_nodes` (clamped to `[1,200]`) by weighted degree
    /// (sum of incident edge weights). Excludes `is_hidden` and `is_self`.
    /// When `project_id` is given only contacts linked to that project (links
    /// relation 'involves') are eligible; edges are filtered so both endpoints
    /// must be in the selected set.
    ///
    /// Returns `{"nodes":[{id,label,message_count}], "edges":[{a,b,weight}]}`.
    pub fn contact_graph(
        &self,
        project_id: Option<&str>,
        max_nodes: i64,
    ) -> Result<serde_json::Value, DbError> {
        let max_nodes = max_nodes.clamp(1, 200);
        let conn = self.lock_conn();

        // 1. Determine the eligible contact pool (project-filtered or all).
        // Each arm collects fully before the stmt is dropped — rusqlite requires
        // the Statement outlive the MappedRows iterator, so we collect early.
        let pool: HashSet<i64> = match project_id {
            Some(pid) => {
                // Exclude hidden/self here (not just at label time) so they can
                // never rank into the top-N or leak into the edge query.
                let mut stmt = conn.prepare(
                    "SELECT c.id
                     FROM links pc
                     JOIN contacts c ON c.id = CAST(pc.dst_id AS INTEGER)
                     WHERE pc.src_type = 'project' AND pc.src_id = ?1
                       AND pc.relation = 'involves' AND pc.dst_type = 'contact'
                       AND c.is_hidden = 0 AND c.is_self = 0",
                )?;
                let ids: Result<HashSet<i64>, _> =
                    stmt.query_map(params![pid], |r| r.get(0))?.collect();
                ids?
            }
            None => {
                // All non-hidden, non-self contacts.
                let mut stmt =
                    conn.prepare("SELECT id FROM contacts WHERE is_hidden = 0 AND is_self = 0")?;
                let ids: Result<HashSet<i64>, _> = stmt.query_map([], |r| r.get(0))?.collect();
                ids?
            }
        };

        if pool.is_empty() {
            return Ok(serde_json::json!({"nodes": [], "edges": []}));
        }

        // 2. Compute weighted degree for each node in the pool.
        //    We iterate edges once and accumulate; only edges where BOTH endpoints
        //    are in the pool count.
        let mut degree: HashMap<i64, f64> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT contact_a, contact_b, weight
                 FROM contact_edges
                 ORDER BY contact_a, contact_b",
            )?;
            let edges_raw: Vec<(i64, i64, f64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;

            for (a, b, w) in &edges_raw {
                if pool.contains(a) && pool.contains(b) {
                    *degree.entry(*a).or_default() += w;
                    *degree.entry(*b).or_default() += w;
                }
            }
        }

        // 3. Select top-N nodes by weighted degree.  For project-filter mode we
        //    also include pool members with degree 0 (they have no edges but they
        //    are legitimately in the project).  Excluded from global mode: nodes
        //    with no edges at all (degree map won't have them) are not interesting.
        let mut ranked: Vec<(i64, f64)> = if project_id.is_some() {
            pool.iter()
                .map(|&id| (id, *degree.get(&id).unwrap_or(&0.0)))
                .collect()
        } else {
            degree.iter().map(|(&id, &w)| (id, w)).collect()
        };
        ranked.sort_unstable_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0)) // stable tie-break on id
        });
        ranked.truncate(max_nodes as usize);
        let selected: HashSet<i64> = ranked.iter().map(|(id, _)| *id).collect();

        // 4. Fetch contact detail for the selected set to build node labels.
        struct NodeRow {
            id: i64,
            email: String,
            derived_name: Option<String>,
            display_name: Option<String>,
            inbound: i64,
            outbound: i64,
        }
        let mut stmt = conn.prepare(
            "SELECT id, email, derived_name, display_name, inbound_count, outbound_count
             FROM contacts
             WHERE id IN (SELECT value FROM json_each(?1))
               AND is_hidden = 0 AND is_self = 0",
        )?;
        let id_json = serde_json::to_string(&selected.iter().copied().collect::<Vec<_>>())
            .map_err(|e| DbError(format!("json: {e}")))?;
        let nodes_raw: Vec<NodeRow> = stmt
            .query_map(params![id_json], |r| {
                Ok(NodeRow {
                    id: r.get(0)?,
                    email: r.get(1)?,
                    derived_name: r.get(2)?,
                    display_name: r.get(3)?,
                    inbound: r.get(4)?,
                    outbound: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Order by the ranked order (descending weighted degree).
        let rank_pos: HashMap<i64, usize> = ranked
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i))
            .collect();
        let mut nodes_raw = nodes_raw;
        nodes_raw.sort_by_key(|r| rank_pos.get(&r.id).copied().unwrap_or(usize::MAX));

        let nodes: Vec<serde_json::Value> = nodes_raw
            .iter()
            .map(|r| {
                let label = r
                    .display_name
                    .as_deref()
                    .or(r.derived_name.as_deref())
                    .unwrap_or(r.email.as_str())
                    .to_owned();
                serde_json::json!({
                    "id": r.id,
                    "label": label,
                    "message_count": r.inbound + r.outbound,
                })
            })
            .collect();

        // 5. Collect edges between the nodes we actually emit.  Built from
        //    nodes_raw (post hidden/self filter), NOT from `selected`, so an
        //    edge can never reference a node absent from the output.
        let node_ids_json =
            serde_json::to_string(&nodes_raw.iter().map(|r| r.id).collect::<Vec<_>>())
                .map_err(|e| DbError(format!("json: {e}")))?;
        let mut stmt = conn.prepare(
            "SELECT contact_a, contact_b, weight
             FROM contact_edges
             WHERE contact_a IN (SELECT value FROM json_each(?1))
               AND contact_b IN (SELECT value FROM json_each(?1))
             ORDER BY weight DESC, contact_a, contact_b",
        )?;
        let edges: Vec<serde_json::Value> = stmt
            .query_map(params![node_ids_json], |r| {
                let a: i64 = r.get(0)?;
                let b: i64 = r.get(1)?;
                let w: f64 = r.get(2)?;
                Ok(serde_json::json!({"a": a, "b": b, "weight": w}))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(serde_json::json!({"nodes": nodes, "edges": edges}))
    }
}

fn row_to_contact(r: &rusqlite::Row<'_>) -> Result<ContactRecord, rusqlite::Error> {
    Ok(ContactRecord {
        id: r.get(0)?,
        email: r.get(1)?,
        derived_name: r.get(2)?,
        display_name: r.get(3)?,
        notes: r.get(4)?,
        is_hidden: r.get(5)?,
        is_self: r.get(6)?,
        is_user_created: r.get(7)?,
        first_seen_at: r.get(8)?,
        last_seen_at: r.get(9)?,
        inbound_count: r.get(10)?,
        outbound_count: r.get(11)?,
    })
}

/// Aggregate contacts from `message_addresses`, pure SQL (read on a snapshot;
/// `UPSERT_CONTACT_ROW_SQL` writes the rows that changed). Best-pick derived_name
/// = the most frequent non-empty name for the address, ties broken by the most
/// recent message carrying it, then by the byte-wise first name (so a full tie
/// never depends on scan order).
///
/// Shape (S64, launch data path -- `bootstrap()` runs this on every launch):
/// ONE pass over the address rows, grouped per (email, name), then rolled up
/// per email. `messages CROSS JOIN message_addresses` pins the join order
/// (SQLite never reorders a CROSS JOIN): a sequential scan of `messages`,
/// then an `idx_message_addresses_msg` seek per message. The planner's own
/// choice drove from the address index instead, paying two B-tree seeks into
/// `messages` per address row, and a correlated derived-name subquery walked
/// every address row a second time. On the 100k perf profile that was
/// ~550 ms of a ~900 ms refresh; this shape measured ~155 ms in an
/// interleaved A/B on the same database, with identical contacts rows
/// (project-docs/perf/read-path-budgets.md).
const AGGREGATE_CONTACTS_SQL: &str = "WITH per_name AS MATERIALIZED (
             SELECT lower(trim(ma.address)) AS email,
                    ma.name AS name,
                    COUNT(*) AS n,
                    MIN(m.sent_at) AS first_seen,
                    MAX(m.sent_at) AS last_seen,
                    SUM(CASE WHEN ma.kind = 'from' THEN 1 ELSE 0 END) AS inbound,
                    SUM(CASE WHEN ma.kind IN ('to','cc','bcc')
                              AND m.from_address IS NOT NULL
                              AND lower(trim(m.from_address)) IN (SELECT lower(email) FROM accounts)
                             THEN 1 ELSE 0 END) AS outbound
             FROM messages m
             CROSS JOIN message_addresses ma ON ma.message_id = m.id
             WHERE trim(ma.address) != '' AND instr(ma.address, '@') > 0
             GROUP BY 1, 2
         ),
         best_name AS (
             SELECT email, name FROM (
                 SELECT email, name,
                        ROW_NUMBER() OVER (
                            PARTITION BY email ORDER BY n DESC, last_seen DESC, name
                        ) AS rank
                 FROM per_name
                 WHERE name IS NOT NULL AND trim(name) != ''
             )
             WHERE rank = 1
         )
         SELECT
             p.email,
             b.name,
             CASE WHEN p.email IN (SELECT lower(email) FROM accounts) THEN 1 ELSE 0 END,
             MIN(p.first_seen),
             MAX(p.last_seen),
             SUM(p.inbound),
             SUM(p.outbound)
         FROM per_name p
         LEFT JOIN best_name b ON b.email = p.email
         GROUP BY p.email";

/// Upsert of one aggregated contact row (S72: the aggregate is read on a
/// snapshot, then written in chunks with this).
const UPSERT_CONTACT_ROW_SQL: &str = "INSERT INTO contacts
             (email, derived_name, is_self, first_seen_at, last_seen_at,
              inbound_count, outbound_count, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))
         ON CONFLICT(email) DO UPDATE SET
             derived_name   = excluded.derived_name,
             is_self        = excluded.is_self,
             first_seen_at  = excluded.first_seen_at,
             last_seen_at   = excluded.last_seen_at,
             inbound_count  = excluded.inbound_count,
             outbound_count = excluded.outbound_count,
             updated_at     = excluded.updated_at";

/// Most rows one refresh write transaction touches before it releases the
/// shared connection (S72). At ~5-20 µs per upsert that is at most ~20 ms of
/// hold (the 100k bench measured 0-7 ms), so a sync batch or a UI read waits
/// at most about that long.
const WRITE_CHUNK: usize = 1_000;

/// Longest single hold of the shared connection within one refresh.
#[derive(Default)]
struct HoldTimer {
    max: Duration,
}

impl HoldTimer {
    fn note(&mut self, acquired: Instant) {
        self.max = self.max.max(acquired.elapsed());
    }
}

/// One aggregated contact, as `AGGREGATE_CONTACTS_SQL` returns it.
struct ContactAggRow {
    email: String,
    derived_name: Option<String>,
    is_self: i64,
    first_seen_at: Option<i64>,
    last_seen_at: Option<i64>,
    inbound: i64,
    outbound: i64,
}

fn read_contact_aggregates(conn: &Connection) -> Result<Vec<ContactAggRow>, DbError> {
    let mut stmt = conn.prepare(AGGREGATE_CONTACTS_SQL)?;
    let rows = stmt.query_map([], |r| {
        Ok(ContactAggRow {
            email: r.get(0)?,
            derived_name: r.get(1)?,
            is_self: r.get(2)?,
            first_seen_at: r.get(3)?,
            last_seen_at: r.get(4)?,
            inbound: r.get(5)?,
            outbound: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// A contact's derived columns: (derived_name, is_self, first_seen_at,
/// last_seen_at, inbound_count, outbound_count).
type Derived = (Option<String>, i64, Option<i64>, Option<i64>, i64, i64);

impl ContactAggRow {
    fn derived(&self) -> Derived {
        (
            self.derived_name.clone(),
            self.is_self,
            self.first_seen_at,
            self.last_seen_at,
            self.inbound,
            self.outbound,
        )
    }
}

/// Every stored contact's derived columns by email (snapshot side of the
/// contacts difference).
fn read_contact_derived(conn: &Connection) -> Result<HashMap<String, Derived>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT email, derived_name, is_self, first_seen_at, last_seen_at,
                inbound_count, outbound_count
         FROM contacts",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            (
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ),
        ))
    })?;
    Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
}

/// Upsert one chunk of aggregated contacts. The conflict clause assigns ONLY
/// derived columns; display_name/notes/is_hidden/is_user_created are
/// user-owned.
fn upsert_contact_rows(tx: &Transaction<'_>, rows: &[&ContactAggRow]) -> Result<(), DbError> {
    let mut stmt = tx.prepare_cached(UPSERT_CONTACT_ROW_SQL)?;
    for r in rows {
        stmt.execute(params![
            r.email,
            r.derived_name,
            r.is_self,
            r.first_seen_at,
            r.last_seen_at,
            r.inbound,
            r.outbound
        ])?;
    }
    Ok(())
}

/// `prune_orphaned_contacts`' "no backing mail" predicate over `contacts`,
/// with the same address-validity filter as the upsert. A per-contact seek
/// on migration 0036's `lower(trim(address))` index (S64); the `NOT IN
/// (SELECT ...)` it replaces listed every address row, twice per refresh
/// (~46 ms -> ~1 ms per statement at 100k).
/// The unary `+` strips `contacts.email`'s TEXT affinity: comparing a column
/// with affinity against the indexed expression makes SQLite apply that
/// affinity to the expression, which rules the index out (it scanned).
/// `address` is NOT NULL, so NOT EXISTS and the old NOT IN agree exactly.
const NO_BACKING_MAIL: &str = "NOT EXISTS (
             SELECT 1 FROM message_addresses ma
             WHERE lower(trim(ma.address)) = +contacts.email
               AND trim(ma.address) != '' AND instr(ma.address, '@') > 0
         )";
/// A manual link in either endpoint position makes the contact user-owned.
const NO_MANUAL_LINK: &str = "NOT EXISTS (
             SELECT 1 FROM links
             WHERE origin = 'manual'
               AND ((src_type = 'contact' AND src_id = CAST(contacts.id AS TEXT))
                 OR (dst_type = 'contact' AND dst_id = CAST(contacts.id AS TEXT)))
         )";

/// Reconcile contacts whose email no longer appears in `message_addresses`
/// (sync hard-deletes message rows on delete→Trash / empty_trash / UIDVALIDITY
/// purge, so the upsert alone would leave stale counts forever):
///   * pure-derived ghosts (no user data at all) are deleted outright;
///   * contacts carrying user data (or user-created ones) survive, but their
///     derived columns are zeroed so the UI never shows phantom history.
///
/// A *manual* link (origin 'manual') to a contact is user-owned relationship
/// data — a manually-assigned project member must not vanish just because its
/// evidencing mail was expunged. Such contacts are preserved (and their
/// derived history zeroed) exactly like ones carrying a display name or note.
/// Derived-only links do NOT protect a contact: the cascade trigger removes
/// them with the ghost, which is correct.
///
/// Uses the same address-validity filter as the upsert.
fn prune_orphaned_contacts(tx: &Transaction<'_>, ids: &[i64]) -> Result<(), DbError> {
    let mut del = tx.prepare(&format!(
        "DELETE FROM contacts
         WHERE id = ?1 AND {NO_BACKING_MAIL}
           AND display_name IS NULL AND notes = ''
           AND is_hidden = 0 AND is_user_created = 0
           AND {NO_MANUAL_LINK}"
    ))?;
    let mut zero = tx.prepare(&format!(
        "UPDATE contacts SET
             derived_name = NULL,
             first_seen_at = NULL,
             last_seen_at = NULL,
             inbound_count = 0,
             outbound_count = 0,
             updated_at = datetime('now')
         WHERE id = ?1 AND {NO_BACKING_MAIL}
           AND (derived_name IS NOT NULL OR first_seen_at IS NOT NULL
                OR last_seen_at IS NOT NULL OR inbound_count != 0
                OR outbound_count != 0)"
    ))?;
    for id in ids {
        if del.execute(params![id])? == 0 {
            zero.execute(params![id])?;
        }
    }
    Ok(())
}

/// Contacts with no backing mail on the snapshot -- the prune's candidates
/// (the same seek `contacts_prune_seeks_the_lower_address_index` pins).
fn read_orphan_candidates(conn: &Connection) -> Result<Vec<i64>, DbError> {
    let mut stmt = conn.prepare(&format!("SELECT id FROM contacts WHERE {NO_BACKING_MAIL}"))?;
    let ids = stmt.query_map([], |r| r.get(0))?;
    Ok(ids.collect::<Result<Vec<_>, _>>()?)
}

/// Compute the whole `contact_edges` graph from mail: same-message co-occurrence of
/// from+to+cc participants (bcc excluded — invisible to the others; is_self
/// excluded — the user hubs everything; >25-participant messages skipped),
/// plus a 0.25× bonus for pairs that share a thread but never a message
/// (per thread, decayed by the thread's most recent message).
fn compute_edges(
    conn: &Connection,
    now_epoch: i64,
) -> Result<HashMap<(i64, i64), EdgeAccum>, DbError> {
    // One indexed scan: every non-self edge participant per message.
    struct Row {
        message_id: String,
        sent_at: Option<i64>,
        thread_id: Option<String>,
        contact_id: i64,
    }
    let mut rows: Vec<Row> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT m.id, m.sent_at, m.thread_id, c.id
             FROM messages m
             JOIN message_addresses ma ON ma.message_id = m.id
             JOIN contacts c ON c.email = lower(trim(ma.address))
             WHERE ma.kind IN ('from','to','cc') AND c.is_self = 0
             ORDER BY m.id",
        )?;
        let it = stmt.query_map([], |r| {
            Ok(Row {
                message_id: r.get(0)?,
                sent_at: r.get(1)?,
                thread_id: r.get(2)?,
                contact_id: r.get(3)?,
            })
        })?;
        for r in it {
            rows.push(r?);
        }
    }

    // Group into per-message participant sets (deduped: the same address may
    // appear under several kinds on one message).
    struct MsgInfo {
        sent_at: Option<i64>,
        thread_id: Option<String>,
        participants: HashSet<i64>,
    }
    let mut messages: HashMap<String, MsgInfo> = HashMap::new();
    for r in rows {
        let e = messages.entry(r.message_id).or_insert_with(|| MsgInfo {
            sent_at: r.sent_at,
            thread_id: r.thread_id,
            participants: HashSet::new(),
        });
        e.participants.insert(r.contact_id);
    }

    let mut edges: HashMap<(i64, i64), EdgeAccum> = HashMap::new();
    // Per-thread state for the thread-only bonus.
    struct ThreadInfo {
        participants: HashSet<i64>,
        same_msg_pairs: HashSet<(i64, i64)>,
        last_sent: Option<i64>,
    }
    let mut threads: HashMap<String, ThreadInfo> = HashMap::new();

    for info in messages.values() {
        // Mailing-list guard applies to both same-message and thread edges.
        if info.participants.len() > MAX_EDGE_PARTICIPANTS {
            continue;
        }
        let mut p: Vec<i64> = info.participants.iter().copied().collect();
        p.sort_unstable();
        let w = decay(now_epoch, info.sent_at);
        for i in 0..p.len() {
            for j in (i + 1)..p.len() {
                let key = pair(p[i], p[j]);
                let acc = edges.entry(key).or_default();
                acc.weight += w;
                acc.message_count += 1;
                acc.touch(info.sent_at);
            }
        }
        if let Some(tid) = &info.thread_id {
            let t = threads.entry(tid.clone()).or_insert_with(|| ThreadInfo {
                participants: HashSet::new(),
                same_msg_pairs: HashSet::new(),
                last_sent: None,
            });
            t.participants.extend(p.iter().copied());
            for i in 0..p.len() {
                for j in (i + 1)..p.len() {
                    t.same_msg_pairs.insert(pair(p[i], p[j]));
                }
            }
            if let Some(s) = info.sent_at {
                t.last_sent = Some(t.last_sent.map_or(s, |cur| cur.max(s)));
            }
        }
    }

    // Thread-only bonus: pairs that share the thread but no message within it.
    for t in threads.values() {
        let mut p: Vec<i64> = t.participants.iter().copied().collect();
        p.sort_unstable();
        let w = THREAD_FACTOR * decay(now_epoch, t.last_sent);
        for i in 0..p.len() {
            for j in (i + 1)..p.len() {
                let key = pair(p[i], p[j]);
                if t.same_msg_pairs.contains(&key) {
                    continue; // already counted as a shared message in this thread
                }
                let acc = edges.entry(key).or_default();
                acc.weight += w;
                acc.touch(t.last_sent);
            }
        }
    }

    Ok(edges)
}

/// A stored edge's (weight, message_count, last_seen_at).
type StoredEdge = (f64, i64, Option<i64>);

/// Every stored edge (snapshot side of the edge difference).
fn read_edges(conn: &Connection) -> Result<HashMap<(i64, i64), StoredEdge>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT contact_a, contact_b, weight, message_count, last_seen_at FROM contact_edges",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?, r.get(4)?)))
    })?;
    Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
}

/// Relative weight change below which an otherwise-identical edge is not
/// rewritten (S72). Every weight decays with `now_epoch`, so without this a
/// refresh rewrote every edge every time -- 33k rows on the 100k perf
/// profile. 1e-3 is the decay of about two hours (90-day time constant):
/// ranking by weight cannot notice it, and the next refresh past that
/// drift rewrites the row.
const EDGE_WEIGHT_REL_TOLERANCE: f64 = 1e-3;

/// Does the computed edge differ from the stored one enough to rewrite?
fn edge_moved(old: &StoredEdge, new: &EdgeAccum) -> bool {
    let (weight, message_count, last_seen_at) = *old;
    message_count != new.message_count
        || last_seen_at != new.last_seen_at
        || (new.weight - weight).abs()
            > EDGE_WEIGHT_REL_TOLERANCE * weight.abs().max(f64::MIN_POSITIVE)
}

fn delete_edges(tx: &Transaction<'_>, keys: &[(i64, i64)]) -> Result<(), DbError> {
    let mut stmt =
        tx.prepare_cached("DELETE FROM contact_edges WHERE contact_a = ?1 AND contact_b = ?2")?;
    for k in keys {
        stmt.execute(params![k.0, k.1])?;
    }
    Ok(())
}

/// Upsert one chunk of computed edges. A contact deleted since the snapshot
/// (a user action, or the FK cascade) makes its edges a no-op instead of an
/// FK failure: the `WHERE EXISTS` pair skips them.
fn upsert_edges(
    tx: &Transaction<'_>,
    keys: &[(i64, i64)],
    edges: &HashMap<(i64, i64), EdgeAccum>,
) -> Result<(), DbError> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO contact_edges (contact_a, contact_b, weight, message_count, last_seen_at)
         SELECT ?1, ?2, ?3, ?4, ?5
         WHERE EXISTS (SELECT 1 FROM contacts WHERE id = ?1)
           AND EXISTS (SELECT 1 FROM contacts WHERE id = ?2)
         ON CONFLICT(contact_a, contact_b) DO UPDATE SET
             weight = excluded.weight,
             message_count = excluded.message_count,
             last_seen_at = excluded.last_seen_at",
    )?;
    for k in keys {
        let acc = &edges[k];
        stmt.execute(params![
            k.0,
            k.1,
            acc.weight,
            acc.message_count,
            acc.last_seen_at
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Address, DraftAddress, Mailbox, NewDraft, NewMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-contacts-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    /// Account whose own address is test@local.test (drives is_self/outbound).
    fn seed_account(db: &Database) {
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: Some("Test".into()),
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
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

    fn addr(kind: &str, name: Option<&str>, address: &str) -> Address {
        Address {
            kind: kind.into(),
            name: name.map(str::to_owned),
            address: address.into(),
        }
    }

    /// A message with explicit participants. The first `from` address (if any)
    /// also becomes `from_address` (drives outbound_count attribution).
    fn msg_with(uid: i64, sent_at: i64, addresses: Vec<Address>) -> NewMessage {
        let from = addresses.iter().find(|a| a.kind == "from");
        NewMessage {
            id: format!("acc1:INBOX:{uid}"),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@local.test>")),
            subject: Some(format!("Subject {uid}")),
            from_name: from.and_then(|a| a.name.clone()),
            from_address: from.map(|a| a.address.clone()),
            sent_at: Some(sent_at),
            snippet: None,
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses,
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: Vec::new(),
        }
    }

    fn contact_by_email(db: &Database, email: &str) -> ContactRecord {
        db.list_contacts(Some(email), true, 10)
            .unwrap()
            .into_iter()
            .find(|c| c.email == email)
            .unwrap_or_else(|| panic!("contact {email} not found"))
    }

    fn draft_to(db: &Database, to: &[(&str, Option<&str>)], cc: &[&str]) -> i64 {
        let da = |(e, n): &(&str, Option<&str>)| DraftAddress {
            name: n.map(str::to_owned),
            email: (*e).into(),
        };
        db.save_draft(&NewDraft {
            id: None,
            account_id: "acc1".into(),
            to: to.iter().map(da).collect(),
            cc: cc
                .iter()
                .map(|e| DraftAddress {
                    name: None,
                    email: (*e).into(),
                })
                .collect(),
            bcc: vec![],
            subject: "s".into(),
            body_text: "b".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        })
        .unwrap()
    }

    #[test]
    fn sent_recipients_cover_sent_and_outbox_not_plain_drafts() {
        let db = fresh_db();
        seed_account(&db);
        let sent = draft_to(
            &db,
            &[
                ("Alice@Local.Test", Some("Alice A")),
                ("test@local.test", None),
            ],
            &["cc@x.test"],
        );
        db.mark_draft_sent(sent).unwrap();
        // Still a plain draft: never sent, so not a known recipient.
        draft_to(&db, &[("unsent@x.test", None)], &[]);

        let got = db.list_sent_recipients(10).unwrap();
        let emails: Vec<&str> = got.iter().map(|r| r.email.as_str()).collect();
        assert!(
            emails.contains(&"alice@local.test"),
            "lower-cased: {emails:?}"
        );
        assert!(emails.contains(&"cc@x.test"));
        assert!(!emails.contains(&"unsent@x.test"));
        assert!(!emails.contains(&"test@local.test"), "own account excluded");
        let alice = got.iter().find(|r| r.email == "alice@local.test").unwrap();
        assert_eq!(alice.name.as_deref(), Some("Alice A"));
        assert_eq!(alice.sent_count, 1);
    }

    #[test]
    fn sent_recipients_exclude_hidden_contacts() {
        let db = fresh_db();
        seed_account(&db);
        let id = draft_to(&db, &[("shown@x.test", None), ("hidden@x.test", None)], &[]);
        db.mark_draft_sent(id).unwrap();
        db.create_contact("hidden@x.test", None).unwrap();
        let c = contact_by_email(&db, "hidden@x.test");
        db.update_contact_user_fields(c.id, None, String::new(), true)
            .unwrap();
        let got = db.list_sent_recipients(10).unwrap();
        let emails: Vec<&str> = got.iter().map(|r| r.email.as_str()).collect();
        assert_eq!(emails, vec!["shown@x.test"]);
    }

    #[test]
    fn sent_recipients_dedupe_count_and_respect_limit() {
        let db = fresh_db();
        seed_account(&db);
        for _ in 0..3 {
            let id = draft_to(&db, &[("bob@x.test", None)], &[]);
            db.mark_draft_sent(id).unwrap();
        }
        let id = draft_to(
            &db,
            &[("bob@x.test", Some("Bob B")), ("zed@x.test", None)],
            &[],
        );
        db.mark_draft_sent(id).unwrap();
        let got = db.list_sent_recipients(10).unwrap();
        assert_eq!(got.len(), 2);
        let bob = got.iter().find(|r| r.email == "bob@x.test").unwrap();
        assert_eq!(bob.sent_count, 4);
        assert_eq!(
            bob.name.as_deref(),
            Some("Bob B"),
            "a later name fills the gap"
        );
        assert_eq!(db.list_sent_recipients(1).unwrap().len(), 1);
    }

    #[test]
    fn refresh_creates_contacts_from_addresses() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW - DAY,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", None, "test@local.test"),
                addr("cc", Some("Bob"), "bob@x.test"),
            ],
        ))
        .unwrap();
        let report = db.refresh_contact_aggregates(NOW).unwrap();
        assert_eq!(report.contacts_upserted, 3);
        let all = db.list_contacts(None, true, 100).unwrap();
        assert_eq!(all.len(), 3);
        let alice = contact_by_email(&db, "alice@x.test");
        assert_eq!(alice.derived_name.as_deref(), Some("Alice"));
        assert_eq!(alice.inbound_count, 1);
        assert_eq!(alice.first_seen_at, Some(NOW - DAY));
        assert_eq!(alice.last_seen_at, Some(NOW - DAY));
    }

    #[test]
    fn list_contacts_orders_recent_correspondents_first() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW - 30 * DAY,
            vec![
                addr("from", Some("Older"), "older@x.test"),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();
        db.upsert_message(&msg_with(
            2,
            NOW,
            vec![
                addr("from", Some("Recent"), "recent@x.test"),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();

        db.refresh_contact_aggregates(NOW).unwrap();
        let contacts = db.list_contacts(None, false, 100).unwrap();
        let recent = contacts
            .iter()
            .position(|contact| contact.email == "recent@x.test")
            .expect("recent correspondent");
        let older = contacts
            .iter()
            .position(|contact| contact.email == "older@x.test")
            .expect("older correspondent");

        assert!(recent < older, "recent correspondents should appear first");
    }

    #[test]
    fn refresh_is_idempotent_and_updates_counts() {
        let db = fresh_db();
        seed_account(&db);
        // Inbound from alice, plus an outbound message from us to bob.
        db.upsert_message(&msg_with(
            1,
            NOW - DAY,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();
        db.upsert_message(&msg_with(
            2,
            NOW,
            vec![
                addr("from", Some("Test"), "test@local.test"),
                addr("to", Some("Bob"), "bob@x.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let first = db.list_contacts(None, true, 100).unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let second = db.list_contacts(None, true, 100).unwrap();
        assert_eq!(first.len(), second.len(), "no growth on re-refresh");

        let bob = contact_by_email(&db, "bob@x.test");
        assert_eq!(bob.inbound_count, 0);
        assert_eq!(bob.outbound_count, 1, "we wrote to bob once");
        let alice = contact_by_email(&db, "alice@x.test");
        assert_eq!(alice.inbound_count, 1);
        assert_eq!(alice.outbound_count, 0);
    }

    #[test]
    fn best_name_prefers_most_frequent_nonempty() {
        let db = fresh_db();
        seed_account(&db);
        // "Alice Ada" twice, "Al" once, one empty name — best pick "Alice Ada".
        for (uid, name) in [
            (1, Some("Alice Ada")),
            (2, Some("Al")),
            (3, Some("Alice Ada")),
            (4, None),
        ] {
            db.upsert_message(&msg_with(
                uid,
                NOW - uid * DAY,
                vec![
                    addr("from", name, "alice@x.test"),
                    addr("to", None, "test@local.test"),
                ],
            ))
            .unwrap();
        }
        db.refresh_contact_aggregates(NOW).unwrap();
        let alice = contact_by_email(&db, "alice@x.test");
        assert_eq!(alice.derived_name.as_deref(), Some("Alice Ada"));
    }

    #[test]
    fn best_name_count_tie_goes_to_the_most_recent_message() {
        let db = fresh_db();
        seed_account(&db);
        // One message each: "Alice Old" 3 days ago, "Alice New" yesterday.
        for (uid, name, age) in [(1, "Alice Old", 3), (2, "Alice New", 1)] {
            db.upsert_message(&msg_with(
                uid,
                NOW - age * DAY,
                vec![
                    addr("from", Some(name), "alice@x.test"),
                    addr("to", None, "test@local.test"),
                ],
            ))
            .unwrap();
        }
        db.refresh_contact_aggregates(NOW).unwrap();
        let alice = contact_by_email(&db, "alice@x.test");
        assert_eq!(alice.derived_name.as_deref(), Some("Alice New"));
    }

    #[test]
    fn best_name_full_tie_goes_to_the_first_name_in_order() {
        let db = fresh_db();
        seed_account(&db);
        // Both names once, on the SAME message (From and Cc): equal count,
        // equal most-recent date. The pick is the byte-wise first name, so
        // it never depends on scan order.
        db.upsert_message(&msg_with(
            1,
            NOW - DAY,
            vec![
                addr("from", Some("Bea"), "alice@x.test"),
                addr("cc", Some("Alice"), "alice@x.test"),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let alice = contact_by_email(&db, "alice@x.test");
        assert_eq!(alice.derived_name.as_deref(), Some("Alice"));
    }

    /// `EXPLAIN QUERY PLAN` detail lines for `sql`, joined.
    fn query_plan(db: &Database, sql: &str) -> String {
        let conn = db.lock_conn();
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare EXPLAIN");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .expect("query EXPLAIN")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect EXPLAIN rows");
        rows.join("\n")
    }

    /// S64 (launch data path): the refresh runs on every launch, and at 100k
    /// messages the upsert was ~60 % of it. It must aggregate in ONE pass
    /// driven by `messages` (sequential scan, then an index seek per
    /// message's addresses): the old shape walked every address row twice --
    /// once outside, once more in a derived-name subquery per contact -- with
    /// two B-tree seeks into `messages` per row each time.
    #[test]
    fn contacts_upsert_is_one_pass_driven_by_messages() {
        let db = fresh_db();
        let plan = query_plan(&db, AGGREGATE_CONTACTS_SQL);
        assert!(
            !plan.contains("CORRELATED"),
            "no per-contact correlated subquery:\n{plan}"
        );
        assert!(
            plan.lines().any(|l| l.starts_with("SCAN m")),
            "messages drives the join:\n{plan}"
        );
        assert!(
            plan.contains("idx_message_addresses_msg"),
            "addresses are found by message id:\n{plan}"
        );
    }

    /// S64: the prune's "no backing mail" test must seek migration 0036's
    /// `lower(trim(address))` index once per contact, not build a list of
    /// every address row (twice per refresh: DELETE and UPDATE).
    #[test]
    fn contacts_prune_seeks_the_lower_address_index() {
        let db = fresh_db();
        let plan = query_plan(
            &db,
            &format!("SELECT id FROM contacts WHERE {NO_BACKING_MAIL}"),
        );
        assert!(
            plan.contains("SEARCH ma USING")
                && plan.contains("idx_message_addresses_lower_address"),
            "expected an index seek on lower(trim(address)):\n{plan}"
        );
    }

    #[test]
    fn email_normalized_lowercase_dedups() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW - DAY,
            vec![
                addr("from", Some("Alice"), "  ALICE@X.Test "),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();
        db.upsert_message(&msg_with(
            2,
            NOW,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let hits: Vec<_> = db
            .list_contacts(Some("alice"), true, 10)
            .unwrap()
            .into_iter()
            .filter(|c| c.email == "alice@x.test")
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "case/whitespace variants dedup to one contact"
        );
        assert_eq!(hits[0].inbound_count, 2);
    }

    #[test]
    fn user_display_name_and_notes_survive_refresh() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW - DAY,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let alice = contact_by_email(&db, "alice@x.test");
        db.update_contact_user_fields(
            alice.id,
            Some("Alice (client)".into()),
            "met at conf".into(),
            true,
        )
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let after = db.get_contact_detail(alice.id).unwrap().unwrap();
        assert_eq!(after.display_name.as_deref(), Some("Alice (client)"));
        assert_eq!(after.notes, "met at conf");
        assert!(after.is_hidden, "is_hidden preserved");
        assert_eq!(
            after.derived_name.as_deref(),
            Some("Alice"),
            "derived recomputed"
        );
    }

    #[test]
    fn self_addresses_flagged_and_excluded_from_edges() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW,
            vec![
                addr("from", Some("Test"), "test@local.test"),
                addr("to", Some("Alice"), "alice@x.test"),
                addr("cc", Some("Bob"), "bob@x.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let me = contact_by_email(&db, "test@local.test");
        assert!(me.is_self);
        let edges = db.list_edges(0.0, 100).unwrap();
        assert_eq!(edges.len(), 1, "only alice-bob; self is excluded");
        let alice = contact_by_email(&db, "alice@x.test");
        let bob = contact_by_email(&db, "bob@x.test");
        let (a, b) = pair(alice.id, bob.id);
        assert_eq!((edges[0].contact_a, edges[0].contact_b), (a, b));
        assert!(!edges
            .iter()
            .any(|e| e.contact_a == me.id || e.contact_b == me.id));
    }

    #[test]
    fn edges_canonical_order_no_self_or_dup() {
        let db = fresh_db();
        seed_account(&db);
        // Two messages with the same trio — must still yield 3 unique edges.
        for uid in 1..=2 {
            db.upsert_message(&msg_with(
                uid,
                NOW - uid * DAY,
                vec![
                    addr("from", Some("Alice"), "alice@x.test"),
                    addr("to", Some("Bob"), "bob@x.test"),
                    addr("cc", Some("Carol"), "carol@x.test"),
                    addr("to", None, "test@local.test"),
                ],
            ))
            .unwrap();
        }
        db.refresh_contact_aggregates(NOW).unwrap();
        let edges = db.list_edges(0.0, 100).unwrap();
        assert_eq!(edges.len(), 3, "C(3,2) unique canonical pairs");
        let mut seen = HashSet::new();
        for e in &edges {
            assert!(e.contact_a < e.contact_b, "canonical order");
            assert!(seen.insert((e.contact_a, e.contact_b)), "no duplicates");
            assert_eq!(e.message_count, 2);
        }
    }

    #[test]
    fn edge_weight_same_message_beats_thread_only() {
        let db = fresh_db();
        seed_account(&db);
        // alice+bob share a message; alice+carol only share a thread (carol
        // replies to alice's message without alice on the recipient list).
        let m1 = msg_with(
            1,
            NOW,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", Some("Bob"), "bob@x.test"),
            ],
        );
        let mut m2 = msg_with(
            2,
            NOW,
            vec![
                addr("from", Some("Carol"), "carol@x.test"),
                addr("to", None, "test@local.test"),
            ],
        );
        m2.references = vec!["<1@local.test>".into()];
        db.upsert_message(&m1).unwrap();
        db.upsert_message(&m2).unwrap();
        db.rebuild_threads("acc1").unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();

        let alice = contact_by_email(&db, "alice@x.test");
        let bob = contact_by_email(&db, "bob@x.test");
        let carol = contact_by_email(&db, "carol@x.test");
        let edges = db.list_edges(0.0, 100).unwrap();
        let weight = |x: i64, y: i64| {
            let k = pair(x, y);
            edges
                .iter()
                .find(|e| (e.contact_a, e.contact_b) == k)
                .map(|e| e.weight)
                .unwrap_or_else(|| panic!("edge {k:?} missing"))
        };
        let same_msg = weight(alice.id, bob.id);
        let thread_only = weight(alice.id, carol.id);
        assert!(
            same_msg > thread_only,
            "same-message {same_msg} must beat thread-only {thread_only}"
        );
        // Both at age 0: 1.0 vs 0.25 (bob-carol is also thread-only).
        assert!((same_msg - 1.0).abs() < 1e-9);
        assert!((thread_only - THREAD_FACTOR).abs() < 1e-9);
        // Thread-only pairs contribute no message_count.
        let ac = pair(alice.id, carol.id);
        let e = edges
            .iter()
            .find(|e| (e.contact_a, e.contact_b) == ac)
            .unwrap();
        assert_eq!(e.message_count, 0);
    }

    #[test]
    fn edge_weight_decays_with_age() {
        let db = fresh_db();
        seed_account(&db);
        // Fresh pair vs the same-shape pair 180 days old.
        db.upsert_message(&msg_with(
            1,
            NOW,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", Some("Bob"), "bob@x.test"),
            ],
        ))
        .unwrap();
        db.upsert_message(&msg_with(
            2,
            NOW - 180 * DAY,
            vec![
                addr("from", Some("Dave"), "dave@x.test"),
                addr("to", Some("Erin"), "erin@x.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let edges = db.list_edges(0.0, 100).unwrap();
        assert_eq!(edges.len(), 2);
        let recent = edges.iter().map(|e| e.weight).fold(f64::MIN, f64::max);
        let old = edges.iter().map(|e| e.weight).fold(f64::MAX, f64::min);
        assert!(recent > old, "recent {recent} must outweigh old {old}");
        assert!((recent - 1.0).abs() < 1e-9, "age 0 -> exp(0) = 1");
        let expected_old = (-180.0f64 / DECAY_DAYS).exp();
        assert!(
            (old - expected_old).abs() < 1e-9,
            "exp(-180/90) = {expected_old}"
        );
    }

    #[test]
    fn wide_recipient_messages_skipped() {
        let db = fresh_db();
        seed_account(&db);
        // 26 non-self participants -> mailing-list guard -> no edges at all.
        let mut addresses = vec![addr("from", Some("List"), "list@x.test")];
        for i in 0..25 {
            addresses.push(addr("to", None, &format!("member{i}@x.test")));
        }
        assert_eq!(addresses.len(), 26);
        db.upsert_message(&msg_with(1, NOW, addresses)).unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        assert_eq!(db.list_contacts(None, true, 100).unwrap().len(), 26);
        assert!(db.list_edges(0.0, 1000).unwrap().is_empty());
    }

    #[test]
    fn bcc_not_edged() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", None, "test@local.test"),
                addr("bcc", Some("Bob"), "bob@x.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        // bob exists as a contact (aggregation counts bcc) but has no edges.
        let bob = contact_by_email(&db, "bob@x.test");
        assert!(db.list_edges(0.0, 100).unwrap().is_empty());
        assert_eq!(bob.inbound_count, 0);
    }

    #[test]
    fn create_and_update_contact_roundtrip() {
        let db = fresh_db();
        seed_account(&db);
        let c = db
            .create_contact("  New.Person@X.Test ", Some("New Person"))
            .unwrap();
        assert_eq!(c.email, "new.person@x.test");
        assert!(c.is_user_created);
        assert_eq!(c.display_name.as_deref(), Some("New Person"));
        assert!(db.create_contact("", None).is_err());
        assert!(db.create_contact("not-an-email", None).is_err());
        db.update_contact_user_fields(c.id, None, "n".into(), false)
            .unwrap();
        let got = db.get_contact_detail(c.id).unwrap().unwrap();
        assert_eq!(got.notes, "n");
        assert!(db
            .update_contact_user_fields(999_999, None, String::new(), false)
            .is_err());
        // Hidden filtering: hide, then list without include_hidden.
        db.update_contact_user_fields(c.id, None, String::new(), true)
            .unwrap();
        assert!(!db
            .list_contacts(None, false, 100)
            .unwrap()
            .iter()
            .any(|x| x.id == c.id));
        assert!(db
            .list_contacts(None, true, 100)
            .unwrap()
            .iter()
            .any(|x| x.id == c.id));
    }

    #[test]
    fn refresh_zeroes_counts_for_contact_with_no_remaining_messages() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW - DAY,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", None, "test@local.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let alice = contact_by_email(&db, "alice@x.test");
        assert_eq!(alice.inbound_count, 1);
        // The user annotates alice, then every backing message is hard-deleted
        // (delete->Trash local removal / empty_trash / UIDVALIDITY purge).
        db.update_contact_user_fields(alice.id, Some("Alice (client)".into()), "vip".into(), false)
            .unwrap();
        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();

        let after = db
            .get_contact_detail(alice.id)
            .unwrap()
            .expect("survives: has user data");
        assert_eq!(after.display_name.as_deref(), Some("Alice (client)"));
        assert_eq!(after.notes, "vip");
        assert_eq!(after.inbound_count, 0, "counts zeroed");
        assert_eq!(after.outbound_count, 0);
        assert_eq!(after.derived_name, None, "derived name cleared");
        assert_eq!(after.first_seen_at, None);
        assert_eq!(after.last_seen_at, None);
    }

    #[test]
    fn refresh_removes_pure_derived_contact_with_no_messages() {
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", Some("Bob"), "bob@x.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        assert!(!db.list_contacts(None, true, 100).unwrap().is_empty());
        assert!(!db.list_edges(0.0, 100).unwrap().is_empty());

        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();

        // Pure-derived ghosts (no display_name/notes/hidden/user-created) are
        // deleted outright, and the edge rebuild leaves no dangling edges.
        assert!(
            db.list_contacts(None, true, 100).unwrap().is_empty(),
            "no user data anywhere -> all derived contacts pruned"
        );
        assert!(db.list_edges(0.0, 100).unwrap().is_empty());
    }

    #[test]
    fn manual_link_preserves_contact_but_derived_only_ghost_is_pruned() {
        // A manually-assigned project member is user-owned relationship data:
        // it must survive when its evidencing mail is expunged (counts zeroed),
        // while a contact carrying only a derived link stays a prunable ghost.
        use crate::{EntityKind, LinkEnd, LinkUpsert};
        let db = fresh_db();
        seed_account(&db);
        db.upsert_message(&msg_with(
            1,
            NOW,
            vec![
                addr("from", Some("Alice"), "alice@x.test"),
                addr("to", Some("Bob"), "bob@x.test"),
            ],
        ))
        .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();
        let alice = contact_by_email(&db, "alice@x.test");
        let bob = contact_by_email(&db, "bob@x.test");

        {
            let conn = db.lock_conn();
            conn.execute(
                "INSERT INTO projects (id, name, status, position, created_at, updated_at, last_activity_at)
                 VALUES ('proj1', 'P', 'active', 0, 0, 0, 0)",
                [],
            )
            .unwrap();
        }
        // Alice: user manually links her to the project. Bob: only a derived link.
        db.upsert_link(LinkUpsert {
            src: LinkEnd::new(EntityKind::Project, "proj1"),
            dst: LinkEnd::new(EntityKind::Contact, &alice.id.to_string()),
            relation: "involves",
            origin: "manual",
            provenance: None,
            confidence: None,
        })
        .unwrap();
        db.upsert_link(LinkUpsert {
            src: LinkEnd::new(EntityKind::Project, "proj1"),
            dst: LinkEnd::new(EntityKind::Contact, &bob.id.to_string()),
            relation: "involves",
            origin: "derived",
            provenance: Some("participant"),
            confidence: Some(0.5),
        })
        .unwrap();

        // All backing mail is expunged, then reconcile.
        db.delete_messages_for_mailbox("acc1", "acc1:INBOX")
            .unwrap();
        db.refresh_contact_aggregates(NOW).unwrap();

        // Alice survives (manual link = user-owned), derived history zeroed.
        let after = db
            .get_contact_detail(alice.id)
            .unwrap()
            .expect("manual-linked contact survives pruning");
        assert_eq!(after.inbound_count, 0, "derived counts zeroed");
        assert_eq!(after.derived_name, None);
        // The manual link is intact.
        let alice_links = db
            .links_for(LinkEnd::new(EntityKind::Contact, &alice.id.to_string()))
            .unwrap();
        assert_eq!(alice_links.len(), 1, "manual project link preserved");
        assert_eq!(alice_links[0].origin, "manual");
        // Bob was a pure-derived ghost: pruned, and the cascade removed his link.
        assert!(
            db.get_contact_detail(bob.id).unwrap().is_none(),
            "derived-only contact is still pruned"
        );
        assert!(db
            .links_for(LinkEnd::new(EntityKind::Contact, &bob.id.to_string()))
            .unwrap()
            .is_empty());
    }

    /// Seed `n` messages, each from a distinct sender and cc'ing the next
    /// sender, so the refresh has `n`+ contacts and ~`n` edges to write.
    fn seed_many(db: &Database, n: i64) {
        for i in 0..n {
            db.upsert_message(&msg_with(
                i + 1,
                NOW - i * 60,
                vec![
                    addr(
                        "from",
                        Some(&format!("Sender {i}")),
                        &format!("s{i}@bulk.test"),
                    ),
                    addr("to", None, "test@local.test"),
                    addr("cc", None, &format!("s{}@bulk.test", i + 1)),
                ],
            ))
            .unwrap();
        }
    }

    /// S72: with two first syncs running, the refresh held the
    /// ONE connection for its whole pass (0.7-1.7 s at 100k), so every sync
    /// upsert and every UI read queued behind it. It must now leave the
    /// connection free for other callers for most of the pass: a thread
    /// spinning on a trivial query has to get in many times while one
    /// refresh runs. The old single-guard shape allows at most two (one
    /// racing the refresh's first lock, one racing its end flag).
    #[test]
    fn refresh_yields_the_connection_to_other_callers() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use std::sync::Arc;
        let db = Arc::new(fresh_db());
        seed_account(&db);
        seed_many(&db, 1_500);
        db.refresh_contact_aggregates(NOW).unwrap(); // warm: rows exist

        let running = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let got_in = Arc::new(AtomicUsize::new(0));
        let spinner = {
            let (db, running, done, got_in) = (
                Arc::clone(&db),
                Arc::clone(&running),
                Arc::clone(&done),
                Arc::clone(&got_in),
            );
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    let conn = db.lock_conn();
                    let _: i64 = conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
                    drop(conn);
                    if running.load(Ordering::SeqCst) && !done.load(Ordering::SeqCst) {
                        got_in.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        };
        running.store(true, Ordering::SeqCst);
        let report = db.refresh_contact_aggregates(NOW + DAY).unwrap();
        done.store(true, Ordering::SeqCst);
        spinner.join().unwrap();
        let n = got_in.load(Ordering::SeqCst);
        assert!(
            n >= 10,
            "another caller got the connection only {n} time(s) during a {} ms refresh",
            report.elapsed_ms
        );
        assert!(report.max_lock_hold_ms <= report.elapsed_ms);
    }

    /// The chunked writer must give exactly what the one-transaction rebuild
    /// gave: every contact, and an edge set that loses a pair once the only
    /// message carrying it is gone (while both contacts survive via other mail).
    #[test]
    fn chunked_refresh_writes_every_row_and_drops_stale_edges() {
        let db = fresh_db();
        seed_account(&db);
        seed_many(&db, 1_200);
        let first = db.refresh_contact_aggregates(NOW).unwrap();
        // 1_201 bulk senders/cc's + the account's own address.
        assert_eq!(first.contacts_upserted, 1_202);
        assert_eq!(first.edges_rebuilt, 1_200);
        let edges = db.list_edges(0.0, 100_000).unwrap();
        assert_eq!(edges.len(), 1_200);

        // Message 1 is the only one pairing s0 with s1; s1 still has mail
        // (message 2 is from s1), s0 has none left and is pruned.
        // A local hard delete, as sync's expunge/UIDVALIDITY purge does.
        db.lock_conn()
            .execute("DELETE FROM messages WHERE id = 'acc1:INBOX:1'", [])
            .unwrap();
        let second = db.refresh_contact_aggregates(NOW).unwrap();
        assert_eq!(second.edges_rebuilt, 1_199);
        assert_eq!(db.list_edges(0.0, 100_000).unwrap().len(), 1_199);
        assert!(db
            .list_contacts(Some("s0@bulk.test"), true, 10)
            .unwrap()
            .is_empty());
        assert_eq!(contact_by_email(&db, "s1@bulk.test").inbound_count, 1);
    }
}
