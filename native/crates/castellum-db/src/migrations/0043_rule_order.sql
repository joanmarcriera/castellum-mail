-- 0043: ordered rules with stop-processing and an
-- enabled switch. Existing rows get sort_order 0 (ties break by name, as
-- before), stop_processing 0, enabled 1 = behaviour unchanged. New rows are
-- appended (sort_order = MAX + 1) by Database::upsert_filter.
ALTER TABLE saved_filters ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;
ALTER TABLE saved_filters ADD COLUMN stop_processing INTEGER NOT NULL DEFAULT 0;
ALTER TABLE saved_filters ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
