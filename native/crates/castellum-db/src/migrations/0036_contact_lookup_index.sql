-- S64 perf fix: refresh_contact_aggregates' derived_name subquery in
-- upsert_contacts (contacts.rs) correlates on lower(trim(ma2.address)) with
-- no usable index, so SQLite full-scans message_addresses (joined to
-- messages) once per distinct contact email. At 100k messages that turned a
-- launch-path call into a ~20s stall (see project-docs/perf/read-path-budgets.md).
-- An expression index lets SQLite seek instead of scan: measured 19.0s ->
-- 0.12s upserting contacts from 20k synthetic messages (160x).
CREATE INDEX idx_message_addresses_lower_address
    ON message_addresses (lower(trim(address)));
