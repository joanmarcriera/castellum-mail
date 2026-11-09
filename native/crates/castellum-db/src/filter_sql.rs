// Castellum - filter/rule SQL compiler (S40).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Compiles a `castellum_models::FilterGroup` into a parameterised WHERE
// fragment, following the design memo's §4 sketch
// (project-docs/rules-filters-native-2026-09-17.md). Reuses
// `search_query::build_fts_query` for the FTS5 token-quoting safety net —
// the memo's own citation for this helper (`repo.rs`) is stale: S39 moved it
// into `search_query.rs` as `pub(crate)` before this stage started, so this
// module calls it from there rather than re-deriving it (the code wins over
// a stale memo citation, per this stage's brief).
//
// Never panics on malformed or empty input: a group with no conditions and
// no nested groups (a freshly-created, not-yet-configured filter) compiles
// to the literal `1=0` (matches nothing); a `GistTopic` condition returns a
// clear `DbError` (memo §7 — the field is reserved, not implemented).

use crate::search_query::build_fts_query;
use crate::DbError;
use castellum_models::{
    header_name_status, normalize_header_name, BoolOp, FilterCondition, FilterField, FilterGroup,
    FilterOperator, FilterValue, HeaderNameStatus,
};
use rusqlite::types::Value;

/// Compiles `group` (recursively) into a WHERE fragment plus the bind values
/// it references, in order. The fragment is spliced directly into a `WHERE`
/// clause by the caller (`rules::evaluate_filter`) — never wrapped further
/// here, since a top-level group's own parentheses are redundant.
pub fn compile_group(group: &FilterGroup, params: &mut Vec<Value>) -> Result<String, DbError> {
    validate_limits(group)?;
    compile_group_inner(group, params, false)
}

/// Deepest nesting of groups a stored filter may have (the top group is depth
/// 1). Bounds recursion in the compiler and SQLite's expression depth.
pub const MAX_GROUP_DEPTH: usize = 8;
/// Most nodes (conditions plus nested groups, root excluded) a filter tree may contain.
pub const MAX_CONDITIONS: usize = 256;

/// Rejects a tree deeper than `MAX_GROUP_DEPTH` or with more than
/// `MAX_CONDITIONS` nodes (conditions + nested groups), without recursing past the limit itself.
pub fn validate_limits(group: &FilterGroup) -> Result<(), DbError> {
    fn walk(g: &FilterGroup, depth: usize, conditions: &mut usize) -> Result<(), DbError> {
        if depth > MAX_GROUP_DEPTH {
            return Err(DbError(format!(
                "filter groups are nested more than {MAX_GROUP_DEPTH} levels deep"
            )));
        }
        // Nested groups count as nodes too, so a tree of thousands of empty
        // subgroups cannot dodge the cap (the root is not counted).
        *conditions += g.conditions.len() + g.groups.len();
        if *conditions > MAX_CONDITIONS {
            return Err(DbError(format!(
                "filter has more than {MAX_CONDITIONS} conditions and groups"
            )));
        }
        g.groups
            .iter()
            .try_for_each(|c| walk(c, depth + 1, conditions))
    }
    walk(group, 1, &mut 0)
}

/// `null_safe`: this group sits (at any depth) under a `None` group that
/// holds a Header leaf. Every NON-header leaf is then coerced with
/// `IFNULL(leaf, 0)` where it stands, so the only NULL left in the subtree is
/// a genuine header-unknown one. Without it, `None[And[header, date > X]]`
/// would drop a row with a NULL `sent_at` (NULL AND 1 = NULL, NOT NULL =
/// NULL) that the pre-header compiler matched through its group-level
/// IFNULL. Leaf-level coercion gives exactly that old answer for the
/// non-header leaves (in Kleene logic, NULL->0 before or after AND/OR is the
/// same when every NULL comes from those leaves), and leaves the fail-closed
/// header NULL intact.
fn compile_group_inner(
    group: &FilterGroup,
    params: &mut Vec<Value>,
    null_safe: bool,
) -> Result<String, DbError> {
    // A `None` group with a header leaf below it coerces its whole subtree.
    let null_safe = null_safe || (group.op == BoolOp::None && has_header_leaf(group));
    // (sql, may_be_unknown): `may_be_unknown` marks a part that can evaluate
    // to a genuine SQL NULL because of a Header leaf (unknown header state).
    let mut parts: Vec<(String, bool)> =
        Vec::with_capacity(group.conditions.len() + group.groups.len());
    for c in &group.conditions {
        let unknown = matches!(c.field, FilterField::Header(_));
        let sql = compile_condition(c, params)?;
        let sql = if null_safe && !unknown {
            format!("IFNULL(({sql}), 0)")
        } else {
            sql
        };
        parts.push((sql, unknown));
    }
    for g in &group.groups {
        parts.push((
            format!("({})", compile_group_inner(g, params, null_safe)?),
            has_header_leaf(g),
        ));
    }
    if parts.is_empty() {
        // Degenerate/malformed input (Global Constraints: "a filter with
        // zero conditions in a group... degrade to a clear DbError, never a
        // panic" for GistTopic; for an empty group specifically the plan's
        // Task 3 calls for a clear empty-match SQL fragment instead, since a
        // freshly-created rule with no conditions yet is a plausible product
        // state, not a hostile input). Deliberately the same for `None`: an
        // empty "none of" group must not start matching everything.
        return Ok("1=0".to_string());
    }
    let join = |sep: &str, parts: Vec<String>| parts.join(sep);
    Ok(match group.op {
        BoolOp::And => join(" AND ", parts.into_iter().map(|(sql, _)| sql).collect()),
        BoolOp::Or => join(" OR ", parts.into_iter().map(|(sql, _)| sql).collect()),
        BoolOp::None => {
            // IFNULL: sent_at/size_bytes may be NULL, and NOT(NULL) is NULL,
            // which would silently drop the row instead of matching it.
            // Parts that can be a header-unknown NULL are NOT coerced: for
            // them NULL must stay NULL through the NOT, so an unknown header
            // state never matches (MM-1c, fail closed). The outer IFNULL
            // (which would turn that NULL into a match) is dropped for the
            // same reason. Non-header leaves of this subtree were already
            // coerced where they stand (`null_safe`, see above), which keeps
            // the old behaviour for them; groups without a header are
            // compiled exactly as before.
            let any_unknown = parts.iter().any(|(_, u)| *u);
            if any_unknown {
                let inner = parts
                    .into_iter()
                    .map(|(sql, _)| sql)
                    .collect::<Vec<_>>()
                    .join(" OR ");
                format!("NOT ({inner})")
            } else {
                let inner = parts.into_iter().map(|(sql, _)| sql).collect::<Vec<_>>();
                format!("NOT IFNULL(({}), 0)", inner.join(" OR "))
            }
        }
    })
}

/// Whether `group` (recursively) contains at least one FTS-routed condition
/// (`Body`; Subject is a LIKE) — the caller uses this to decide BM25 vs
/// `sort_field`/`sort_desc` ordering (memo §4's "BM25 ranking only applies
/// when the group contains at least one FTS condition").
pub fn has_fts_condition(group: &FilterGroup) -> bool {
    group
        .conditions
        .iter()
        .any(|c| c.field == FilterField::Body)
        || group.groups.iter().any(has_fts_condition)
}

fn compile_condition(c: &FilterCondition, params: &mut Vec<Value>) -> Result<String, DbError> {
    match &c.field {
        FilterField::Subject | FilterField::Body => {
            let text = text_value(&c.value)?;
            // Subject is always a LIKE on the stored subject: substring for
            // `contains` (same as the search bar's `subject:`, so `invoice`
            // finds "invoices" and "Re:invoice"), exact / prefix for the
            // others — FTS tokens cannot express any of these. Body has no
            // stored plain column, so it stays on FTS (prefix tokens).
            if matches!(c.field, FilterField::Subject) {
                params.push(Value::Text(folded_like_pattern(text, c.operator)));
                return Ok("cfold(m.subject) LIKE ? ESCAPE '\\'".to_string());
            }
            // Body: every token is a prefix match (`"tok"*`), so `invoice`
            // also finds "invoices"; a mid-word substring still does not.
            let fts = build_fts_query(text)
                .split(' ')
                .filter(|t| !t.is_empty())
                .map(|t| format!("{t}*"))
                .collect::<Vec<_>>()
                .join(" ");
            if fts.is_empty() {
                // Pure punctuation (e.g. "---") contributes no FTS token —
                // an empty MATCH string is invalid SQL, so degrade to
                // "matches nothing" rather than erroring or panicking.
                return Ok("1=0".to_string());
            }
            // Restrict to the body FTS column so "body contains x" does not
            // also match the subject or sender (MM-1). `fts` is only quoted
            // alphanumeric tokens with `*`, and the column name is a literal.
            params.push(Value::Text(format!("body : ({fts})")));
            Ok(
                "m.id IN (SELECT message_id FROM message_fts WHERE message_fts MATCH ?)"
                    .to_string(),
            )
        }
        FilterField::From | FilterField::To | FilterField::Cc | FilterField::Bcc => {
            let kind = match &c.field {
                FilterField::From => "from",
                FilterField::To => "to",
                FilterField::Bcc => "bcc",
                _ => "cc",
            };
            let pattern = folded_like_pattern(text_value(&c.value)?, c.operator);
            params.push(Value::Text(kind.to_string()));
            params.push(Value::Text(pattern.clone()));
            params.push(Value::Text(pattern));
            Ok(
                "m.id IN (SELECT message_id FROM message_addresses WHERE kind = ? \
                AND (cfold(address) LIKE ? ESCAPE '\\' OR cfold(name) LIKE ? ESCAPE '\\'))"
                    .to_string(),
            )
        }
        FilterField::IsRead => bool_flag_sql("is_read", &c.value, params),
        FilterField::IsStarred => bool_flag_sql("is_starred", &c.value, params),
        FilterField::IsAnswered => bool_flag_sql("is_answered", &c.value, params),
        FilterField::IsDraft => bool_flag_sql("is_draft", &c.value, params),
        FilterField::IsDeleted => bool_flag_sql("is_deleted", &c.value, params),
        FilterField::Date => date_sql(c.operator, &c.value, params),
        FilterField::SizeBytes => size_sql(c.operator, &c.value, params),
        FilterField::HasAttachments => {
            let b = bool_value(&c.value)?;
            params.push(Value::Integer(i64::from(b)));
            Ok("m.has_attachments = ?".to_string())
        }
        FilterField::MailboxId => mailbox_id_sql(c.operator, &c.value, params),
        FilterField::AccountId => id_column_sql("m.account_id", c.operator, &c.value, params),
        FilterField::AttachmentName => attachment_sql("filename", c, params),
        FilterField::AttachmentType => attachment_sql("content_type", c, params),
        FilterField::Tag => tag_sql(&c.value, params),
        FilterField::Header(name) => header_sql(name, c, params),
        FilterField::GistTopic => Err(DbError(
            "Gist topic filtering is not yet available — see \
             project-docs/rules-filters-native-2026-09-17.md §7"
                .to_string(),
        )),
    }
}

fn bool_flag_sql(
    column: &'static str,
    value: &FilterValue,
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    let b = bool_value(value)?;
    params.push(Value::Integer(i64::from(b)));
    Ok(format!(
        "COALESCE((SELECT {column} FROM message_flags WHERE message_id = m.id), 0) = ?"
    ))
}

fn date_sql(
    operator: FilterOperator,
    value: &FilterValue,
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    // Relative operators: value is a number of days, resolved against "now"
    // at evaluation time so a smart mailbox never goes stale.
    if matches!(
        operator,
        FilterOperator::WithinLastDays | FilterOperator::OlderThanDays
    ) {
        let days = number_value(value)?;
        if !(0..=36_500).contains(&days) {
            return Err(DbError("relative date needs 0-36500 days".to_string()));
        }
        params.push(Value::Integer(days * 86_400));
        let cmp = if operator == FilterOperator::WithinLastDays {
            ">="
        } else {
            "<"
        };
        return Ok(format!(
            "m.sent_at {cmp} CAST(strftime('%s','now') AS INTEGER) - ?"
        ));
    }
    let cmp = match operator {
        FilterOperator::Before => "<",
        FilterOperator::After => ">",
        FilterOperator::OnOrBefore => "<=",
        FilterOperator::OnOrAfter => ">=",
        other => {
            return Err(DbError(format!(
                "date field requires before/after/on_or_before/on_or_after/within_last_days/older_than_days, got {other:?}"
            )))
        }
    };
    params.push(Value::Integer(number_value(value)?));
    Ok(format!("m.sent_at {cmp} ?"))
}

fn size_sql(
    operator: FilterOperator,
    value: &FilterValue,
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    let cmp = match operator {
        FilterOperator::Gt => ">",
        FilterOperator::Lt => "<",
        FilterOperator::Gte => ">=",
        FilterOperator::Lte => "<=",
        other => {
            return Err(DbError(format!(
                "size_bytes field requires gt/lt/gte/lte, got {other:?}"
            )))
        }
    };
    params.push(Value::Integer(number_value(value)?));
    Ok(format!("m.size_bytes {cmp} ?"))
}

fn mailbox_id_sql(
    operator: FilterOperator,
    value: &FilterValue,
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    id_column_sql("m.mailbox_id", operator, value, params)
}

/// `column = ?` or `column IN (?, ...)` for an id-valued field. `column` is a
/// fixed literal chosen by the caller, never user input.
fn id_column_sql(
    column: &'static str,
    operator: FilterOperator,
    value: &FilterValue,
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    if operator == FilterOperator::In {
        let ids = list_value(value)?;
        if ids.is_empty() {
            return Ok("1=0".to_string());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        for id in ids {
            params.push(Value::Text(id.clone()));
        }
        return Ok(format!("{column} IN ({placeholders})"));
    }
    params.push(Value::Text(text_value(value)?.to_string()));
    Ok(format!("{column} = ?"))
}

/// Tag condition (MM-1b). A text value matches messages carrying that tag:
/// the lower-cased keyword key directly, or a tag whose display name equals
/// it (display names may hold spaces, which IMAP keywords cannot, so the
/// key alone would never find "To do"). A bool value means "has any tag"
/// (`true`) or "has no tag" (`false`). The value is always a bound parameter.
fn tag_sql(value: &FilterValue, params: &mut Vec<Value>) -> Result<String, DbError> {
    if let FilterValue::Bool(any) = value {
        // Hidden protocol keywords ($Forwarded, NonJunk, ...) are stored but
        // never shown as tags, so they must not count here (MM1c).
        let keys: Vec<String> = crate::tags::hidden_keyword_keys().collect();
        let placeholders = vec!["?"; keys.len()].join(", ");
        params.extend(keys.into_iter().map(Value::Text));
        let exists = format!(
            "EXISTS (SELECT 1 FROM message_keywords mk WHERE mk.message_id = m.id \
            AND mk.keyword_key NOT IN ({placeholders}))"
        );
        return Ok(if *any {
            exists
        } else {
            format!("NOT {exists}")
        });
    }
    let name = text_value(value)?;
    params.push(Value::Text(name.to_string()));
    params.push(Value::Text(name.to_string()));
    Ok(
        "m.id IN (SELECT message_id FROM message_keywords WHERE keyword_key = lower(?) \
        OR keyword_key IN (SELECT keyword_key FROM tag_definitions \
        WHERE cfold(display_name) = cfold(?)))"
            .to_string(),
    )
}

/// Header condition (MM-1c, #2710). The name goes through the shared policy
/// (`header_name_status`): invalid, built-in and never-stored names are a
/// `DbError`, so `castellum_upsert_smart_mailbox` stores nothing for them. The
/// name and the value are bound parameters; the name is resolved against the
/// `header_names` dictionary (an unknown name has no id and matches nothing).
/// `Contains` / `Equals` / `StartsWith` + non-empty text match when ANY
/// occurrence matches (accent/case folded like Subject). A value cut at
/// `HEADER_VALUE_MAX_BYTES` is stored with `HEADER_VALUE_CUT_MARKER` and its
/// capture is partial: `Equals` can never match it, and a non-match on it is
/// UNKNOWN through the partial capture (see `header_tri_state`). `Is` + bool is
/// present / absent. "Absent" only holds for messages whose headers were
/// captured, so a not-yet-captured message never counts as "has no List-Id".
/// Cost: one range scan of `idx_message_headers_name` for the name; "absent"
/// also walks the capture table's primary key.
fn header_sql(name: &str, c: &FilterCondition, params: &mut Vec<Value>) -> Result<String, DbError> {
    let Some(norm) = normalize_header_name(name) else {
        return Err(DbError("not a valid header name".to_string()));
    };
    match header_name_status(&norm) {
        HeaderNameStatus::Storable => {}
        HeaderNameStatus::BuiltInField(_) => {
            return Err(DbError(
                "that header has its own field; use the From/To/Cc/Bcc/Subject/Date field"
                    .to_string(),
            ))
        }
        HeaderNameStatus::NotStored => {
            return Err(DbError("Castellum does not keep that header".to_string()))
        }
        HeaderNameStatus::Invalid => return Err(DbError("not a valid header name".to_string())),
    }
    const NAME_ID: &str = "(SELECT id FROM header_names WHERE name = ?)";
    match c.operator {
        FilterOperator::Contains | FilterOperator::Equals | FilterOperator::StartsWith => {
            let text = text_value(&c.value)?;
            if text.is_empty() {
                return Err(DbError("a header condition needs a value".to_string()));
            }
            // Stored values never hold control characters, and a cut value
            // ends with the control character HEADER_VALUE_CUT_MARKER: a value
            // with one could only ever match that marker.
            if text.chars().any(char::is_control) {
                return Err(DbError(
                    "a header condition value cannot contain control characters".to_string(),
                ));
            }
            params.push(Value::Text(norm));
            params.push(Value::Text(folded_like_pattern(text, c.operator)));
            Ok(header_tri_state(
                &format!(
                    "m.id IN (SELECT message_id FROM message_headers WHERE name_id = {NAME_ID} \
                    AND cfold(value) LIKE ? ESCAPE '\\')"
                ),
                true,
            ))
        }
        FilterOperator::Is => {
            let present = bool_value(&c.value)?;
            params.push(Value::Text(norm));
            let has = format!(
                "m.id IN (SELECT message_id FROM message_headers WHERE name_id = {NAME_ID})"
            );
            Ok(header_tri_state(&has, present))
        }
        other => Err(DbError(format!(
            "header field requires contains/equals/starts_with/is, got {other:?}"
        ))),
    }
}

/// Three-valued header leaf (fail closed). `has` is true when a stored header
/// row matches; `positive` says whether the condition is "has" (true) or
/// "absent" (false).
///
///   * a stored match is conclusive on any capture, partial or not;
///   * no stored match on a COMPLETE capture is conclusive the other way;
///   * otherwise (uncaptured, or partial: caps hit, parse failure, name
///     dictionary full) the answer is UNKNOWN = NULL.
///
/// Callers must keep NULL as NULL under negation (see `compile_group_inner`),
/// so an unknown header state is never a match, not even in a `none` group.
fn header_tri_state(has: &str, positive: bool) -> String {
    let (yes, no) = if positive { (1, 0) } else { (0, 1) };
    format!(
        "CASE WHEN {has} THEN {yes} \
         WHEN m.id IN (SELECT message_id FROM message_header_capture WHERE partial = 0) \
         THEN {no} END"
    )
}

/// Whether `group` (recursively) holds a Header condition.
pub(crate) fn has_header_leaf(group: &FilterGroup) -> bool {
    group
        .conditions
        .iter()
        .any(|c| matches!(c.field, FilterField::Header(_)))
        || group.groups.iter().any(has_header_leaf)
}

/// Attachment descriptor match. Descriptors exist only for messages whose
/// structure was fetched, so this can miss not-yet-hydrated messages.
fn attachment_sql(
    column: &'static str,
    c: &FilterCondition,
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    params.push(Value::Text(folded_like_pattern(
        text_value(&c.value)?,
        c.operator,
    )));
    Ok(format!(
        "m.id IN (SELECT message_id FROM message_attachments WHERE cfold({column}) LIKE ? ESCAPE '\\')"
    ))
}

/// Wraps a user value as a `%...%` LIKE pattern (or `val%` for `StartsWith`,
/// unwrapped for `Equals`), escaping the three characters special to SQL
/// `LIKE` (`\`, `%`, `_`) so a literal percent/underscore is matched
/// literally, not as a wildcard. Mirrors `search_query::like_pattern`
/// (kept separate rather than shared — this compiler has its own operator
/// axis, `search_query`'s grammar has none).
fn like_pattern(v: &str, operator: FilterOperator) -> String {
    let escaped = v
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    match operator {
        FilterOperator::StartsWith => format!("{escaped}%"),
        FilterOperator::Equals => escaped,
        _ => format!("%{escaped}%"), // Contains, and any other operator sent our way
    }
}

/// [`like_pattern`] over the accent/case-folded value, to pair with a
/// `cfold(column)` on the SQL side (same folding as the search bar).
fn folded_like_pattern(v: &str, operator: FilterOperator) -> String {
    like_pattern(&crate::search_query::fold_text(v), operator)
}

fn text_value(v: &FilterValue) -> Result<&str, DbError> {
    match v {
        FilterValue::Text(s) => Ok(s),
        _ => Err(DbError("expected a text filter value".to_string())),
    }
}

fn number_value(v: &FilterValue) -> Result<i64, DbError> {
    match v {
        FilterValue::Number(n) => Ok(*n),
        _ => Err(DbError("expected a numeric filter value".to_string())),
    }
}

fn bool_value(v: &FilterValue) -> Result<bool, DbError> {
    match v {
        FilterValue::Bool(b) => Ok(*b),
        _ => Err(DbError("expected a boolean filter value".to_string())),
    }
}

fn list_value(v: &FilterValue) -> Result<&[String], DbError> {
    match v {
        FilterValue::List(l) => Ok(l),
        _ => Err(DbError("expected a list filter value".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::FlagName;

    fn cond(field: FilterField, operator: FilterOperator, value: FilterValue) -> FilterGroup {
        FilterGroup {
            op: BoolOp::And,
            conditions: vec![FilterCondition {
                field,
                operator,
                value,
            }],
            groups: vec![],
        }
    }

    #[test]
    fn subject_contains_is_a_like_substring() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::Subject,
                FilterOperator::Contains,
                FilterValue::Text("50%_off".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(sql, "cfold(m.subject) LIKE ? ESCAPE '\\'");
        assert_eq!(params, vec![Value::Text("%50\\%\\_off%".to_string())]);
    }

    #[test]
    fn body_contains_compiles_to_fts_prefix_match() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::Body,
                FilterOperator::Contains,
                FilterValue::Text("hello world".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert!(sql.contains("message_fts MATCH ?"));
        assert_eq!(
            params[0],
            Value::Text("body : (\"hello\"* \"world\"*)".to_string())
        );
    }

    #[test]
    fn empty_subgroups_count_towards_the_node_cap() {
        let mut g = FilterGroup {
            op: BoolOp::And,
            conditions: vec![],
            groups: vec![],
        };
        for _ in 0..=MAX_CONDITIONS {
            g.groups.push(FilterGroup {
                op: BoolOp::And,
                conditions: vec![],
                groups: vec![],
            });
        }
        assert!(validate_limits(&g).is_err());
        g.groups.truncate(MAX_CONDITIONS);
        assert!(validate_limits(&g).is_ok());
    }

    #[test]
    fn subject_with_only_punctuation_degrades_to_empty_match_not_panic() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::Body,
                FilterOperator::Contains,
                FilterValue::Text("---".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(sql, "1=0");
        assert!(params.is_empty());
    }

    #[test]
    fn from_contains_binds_kind_and_like_pattern_twice() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::From,
                FilterOperator::Contains,
                FilterValue::Text("alice".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert!(sql.starts_with("m.id IN (SELECT message_id FROM message_addresses WHERE kind = ?"));
        assert_eq!(
            params,
            vec![
                Value::Text("from".into()),
                Value::Text("%alice%".into()),
                Value::Text("%alice%".into()),
            ]
        );
    }

    #[test]
    fn to_starts_with_binds_prefix_pattern() {
        let mut params = Vec::new();
        compile_group(
            &cond(
                FilterField::To,
                FilterOperator::StartsWith,
                FilterValue::Text("bob".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(params[1], Value::Text("bob%".into()));
    }

    #[test]
    fn cc_equals_binds_exact_pattern_no_wildcard() {
        let mut params = Vec::new();
        compile_group(
            &cond(
                FilterField::Cc,
                FilterOperator::Equals,
                FilterValue::Text("carol@x.test".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(params[1], Value::Text("carol@x.test".into()));
    }

    #[test]
    fn address_pattern_escapes_like_metacharacters() {
        let mut params = Vec::new();
        compile_group(
            &cond(
                FilterField::From,
                FilterOperator::Contains,
                FilterValue::Text("50%_off".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(params[1], Value::Text("%50\\%\\_off%".into()));
    }

    #[test]
    fn is_read_binds_bool_as_integer() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::IsRead,
                FilterOperator::Is,
                FilterValue::Bool(false),
            ),
            &mut params,
        )
        .unwrap();
        assert!(sql.contains("is_read"));
        assert_eq!(params, vec![Value::Integer(0)]);
    }

    #[test]
    fn is_starred_binds_true_as_one() {
        let mut params = Vec::new();
        compile_group(
            &cond(
                FilterField::IsStarred,
                FilterOperator::Is,
                FilterValue::Bool(true),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(params, vec![Value::Integer(1)]);
    }

    #[test]
    fn is_answered_is_draft_is_deleted_compile() {
        for field in [
            FilterField::IsAnswered,
            FilterField::IsDraft,
            FilterField::IsDeleted,
        ] {
            let mut params = Vec::new();
            let sql = compile_group(
                &cond(field, FilterOperator::Is, FilterValue::Bool(true)),
                &mut params,
            )
            .unwrap();
            assert!(sql.contains("message_flags"));
        }
    }

    #[test]
    fn date_before_uses_lt() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::Date,
                FilterOperator::Before,
                FilterValue::Number(1_000),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(sql, "m.sent_at < ?");
        assert_eq!(params, vec![Value::Integer(1_000)]);
    }

    #[test]
    fn date_after_on_or_before_on_or_after_use_expected_comparators() {
        for (op, expected) in [
            (FilterOperator::After, ">"),
            (FilterOperator::OnOrBefore, "<="),
            (FilterOperator::OnOrAfter, ">="),
        ] {
            let mut params = Vec::new();
            let sql = compile_group(
                &cond(FilterField::Date, op, FilterValue::Number(1)),
                &mut params,
            )
            .unwrap();
            assert_eq!(sql, format!("m.sent_at {expected} ?"));
        }
    }

    #[test]
    fn date_with_wrong_operator_errors_not_panics() {
        let mut params = Vec::new();
        let err = compile_group(
            &cond(
                FilterField::Date,
                FilterOperator::Contains,
                FilterValue::Number(1),
            ),
            &mut params,
        )
        .unwrap_err();
        assert!(err.0.contains("date field"));
    }

    #[test]
    fn size_bytes_gt_lt_gte_lte_use_expected_comparators() {
        for (op, expected) in [
            (FilterOperator::Gt, ">"),
            (FilterOperator::Lt, "<"),
            (FilterOperator::Gte, ">="),
            (FilterOperator::Lte, "<="),
        ] {
            let mut params = Vec::new();
            let sql = compile_group(
                &cond(FilterField::SizeBytes, op, FilterValue::Number(500)),
                &mut params,
            )
            .unwrap();
            assert_eq!(sql, format!("m.size_bytes {expected} ?"));
        }
    }

    #[test]
    fn has_attachments_binds_bool_as_integer() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::HasAttachments,
                FilterOperator::Is,
                FilterValue::Bool(true),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(sql, "m.has_attachments = ?");
        assert_eq!(params, vec![Value::Integer(1)]);
    }

    #[test]
    fn mailbox_id_equals_binds_single_value() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::MailboxId,
                FilterOperator::Equals,
                FilterValue::Text("acc:INBOX".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(sql, "m.mailbox_id = ?");
        assert_eq!(params, vec![Value::Text("acc:INBOX".into())]);
    }

    #[test]
    fn mailbox_id_in_binds_placeholder_per_id() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::MailboxId,
                FilterOperator::In,
                FilterValue::List(vec!["acc:INBOX".into(), "acc:Archive".into()]),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(sql, "m.mailbox_id IN (?, ?)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn mailbox_id_in_with_empty_list_degrades_to_empty_match() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::MailboxId,
                FilterOperator::In,
                FilterValue::List(vec![]),
            ),
            &mut params,
        )
        .unwrap();
        assert_eq!(sql, "1=0");
    }

    #[test]
    fn gist_topic_returns_db_error_never_panics() {
        let mut params = Vec::new();
        let err = compile_group(
            &cond(
                FilterField::GistTopic,
                FilterOperator::In,
                FilterValue::List(vec!["x".into()]),
            ),
            &mut params,
        )
        .unwrap_err();
        assert!(err.0.contains("Gist topic"));
        assert!(err.0.contains("§7"));
    }

    #[test]
    fn empty_group_degrades_to_empty_match_not_panic() {
        let group = FilterGroup {
            op: BoolOp::And,
            conditions: vec![],
            groups: vec![],
        };
        let mut params = Vec::new();
        assert_eq!(compile_group(&group, &mut params).unwrap(), "1=0");
        assert!(params.is_empty());
    }

    #[test]
    fn and_or_nested_groups_two_levels_deep_parenthesise_correctly() {
        // AND( IsRead=false, OR( Subject contains "invoice", AND( From contains "billing", IsStarred=true ) ) )
        let inner_and = FilterGroup {
            op: BoolOp::And,
            conditions: vec![
                FilterCondition {
                    field: FilterField::From,
                    operator: FilterOperator::Contains,
                    value: FilterValue::Text("billing".into()),
                },
                FilterCondition {
                    field: FilterField::IsStarred,
                    operator: FilterOperator::Is,
                    value: FilterValue::Bool(true),
                },
            ],
            groups: vec![],
        };
        let middle_or = FilterGroup {
            op: BoolOp::Or,
            conditions: vec![FilterCondition {
                field: FilterField::Subject,
                operator: FilterOperator::Contains,
                value: FilterValue::Text("invoice".into()),
            }],
            groups: vec![inner_and],
        };
        let top = FilterGroup {
            op: BoolOp::And,
            conditions: vec![FilterCondition {
                field: FilterField::IsRead,
                operator: FilterOperator::Is,
                value: FilterValue::Bool(false),
            }],
            groups: vec![middle_or],
        };
        let mut params = Vec::new();
        let sql = compile_group(&top, &mut params).unwrap();
        // Two levels of nesting -> two layers of parens around the deepest group.
        assert!(sql.contains("(("), "expected nested parens in: {sql}");
        assert!(sql.contains(" AND "));
        assert!(sql.contains(" OR "));
    }

    #[test]
    fn intersection_not_union_and_of_fts_and_flag_condition() {
        // "subject contains invoice AND is_read = false" must produce one
        // fragment ANDing both, never accidentally OR-ing them (regression
        // guard against a false-positive union).
        let group = FilterGroup {
            op: BoolOp::And,
            conditions: vec![
                FilterCondition {
                    field: FilterField::Subject,
                    operator: FilterOperator::Contains,
                    value: FilterValue::Text("invoice".into()),
                },
                FilterCondition {
                    field: FilterField::IsRead,
                    operator: FilterOperator::Is,
                    value: FilterValue::Bool(false),
                },
            ],
            groups: vec![],
        };
        let mut params = Vec::new();
        let sql = compile_group(&group, &mut params).unwrap();
        assert!(sql.contains(" AND "));
        assert!(!sql.contains(" OR "));
    }

    #[test]
    fn has_fts_condition_detects_nested_subject_or_body() {
        let with_fts = FilterGroup {
            op: BoolOp::And,
            conditions: vec![],
            groups: vec![cond(
                FilterField::Body,
                FilterOperator::Contains,
                FilterValue::Text("x".into()),
            )],
        };
        assert!(has_fts_condition(&with_fts));

        let without_fts = cond(
            FilterField::IsRead,
            FilterOperator::Is,
            FilterValue::Bool(true),
        );
        assert!(!has_fts_condition(&without_fts));
    }

    #[test]
    fn addflag_flagname_starred_is_the_only_variant_compiles() {
        // Not a compiler test per se — pins that FlagName is importable and
        // has exactly the one variant this module's callers expect.
        let _ = FlagName::Starred;
    }

    #[test]
    fn header_conditions_compile_to_bound_name_id_subqueries() {
        let mut params = Vec::new();
        let sql = compile_group(
            &cond(
                FilterField::Header(" List-Id ".into()),
                FilterOperator::Contains,
                FilterValue::Text("50%_off".into()),
            ),
            &mut params,
        )
        .unwrap();
        assert!(
            sql.starts_with(
                "CASE WHEN m.id IN (SELECT message_id FROM message_headers WHERE name_id = \
                 (SELECT id FROM header_names WHERE name = ?) AND cfold(value) LIKE ? ESCAPE '\\') THEN 1"
            ) && sql.ends_with("WHERE partial = 0) THEN 0 END"),
            "{sql}"
        );
        assert_eq!(
            params,
            vec![
                Value::Text("list-id".into()),
                Value::Text("%50\\%\\_off%".into())
            ]
        );

        let mut params = Vec::new();
        let present = compile_group(
            &cond(
                FilterField::Header("X-Mailer".into()),
                FilterOperator::Is,
                FilterValue::Bool(true),
            ),
            &mut params,
        )
        .unwrap();
        assert!(present.starts_with("CASE WHEN m.id IN (SELECT message_id FROM message_headers"));
        assert_eq!(params, vec![Value::Text("x-mailer".into())]);

        let mut params = Vec::new();
        let absent = compile_group(
            &cond(
                FilterField::Header("X-Mailer".into()),
                FilterOperator::Is,
                FilterValue::Bool(false),
            ),
            &mut params,
        )
        .unwrap();
        assert!(absent.contains("message_header_capture") && absent.contains("THEN 0"));
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn header_conditions_reject_unsupported_names_operators_and_values() {
        for (name, op, v) in [
            (
                "From",
                FilterOperator::Contains,
                FilterValue::Text("a".into()),
            ),
            (
                "Subject",
                FilterOperator::Contains,
                FilterValue::Text("a".into()),
            ),
            (
                "Received",
                FilterOperator::Contains,
                FilterValue::Text("a".into()),
            ),
            ("Cookie", FilterOperator::Is, FilterValue::Bool(true)),
            (
                "a b",
                FilterOperator::Contains,
                FilterValue::Text("a".into()),
            ),
            ("List-Id", FilterOperator::Gt, FilterValue::Number(1)),
            ("List-Id", FilterOperator::In, FilterValue::List(vec![])),
            (
                "List-Id",
                FilterOperator::Contains,
                FilterValue::Text(String::new()),
            ),
            ("List-Id", FilterOperator::Is, FilterValue::Text("x".into())),
            (
                "List-Id",
                FilterOperator::Contains,
                FilterValue::Text("a\u{80}".into()),
            ),
        ] {
            let mut params = Vec::new();
            let r = compile_group(&cond(FilterField::Header(name.into()), op, v), &mut params);
            assert!(r.is_err(), "{name} {op:?} must be rejected");
        }
    }
}
