-- 0019: per-account write policy (fail-closed evaluation mode).
--
-- Castellum defaults every account to READ-ONLY "evaluation mode": no changes
-- are made to the mail server (no STORE/MOVE/COPY/EXPUNGE/APPEND, no SMTP send)
-- until a human explicitly enables writes. `write_enabled` defaults to 0 so
-- BOTH newly-added AND every pre-existing account become read-only on upgrade
-- (global fail-closed). The CHECK makes a malformed value impossible; a missing
-- row (unknown account) is treated as read-only by the accessor.
ALTER TABLE accounts
    ADD COLUMN write_enabled INTEGER NOT NULL DEFAULT 0
        CHECK (write_enabled IN (0, 1));
