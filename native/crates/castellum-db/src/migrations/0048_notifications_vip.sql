-- 0048 (S59; renumbered from 0042 onto the train): VIP senders and the new-mail
-- notification outbox. See project-docs/plans/2026-09-25-notifications-vip.md.
-- (The plan reserved 0040; 0040/0041 were already taken on the stacked base.)
CREATE TABLE vip_senders (
    address_key  TEXT PRIMARY KEY,          -- lower(trim(address)), as contacts.rs keys addresses
    display_name TEXT,
    created_at   INTEGER NOT NULL
) WITHOUT ROWID;

-- Written by the core at the end of a sync pass, drained by the app.
-- message_id is UNIQUE so a message is announced at most once, ever.
CREATE TABLE notification_outbox (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id   TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
    created_at   INTEGER NOT NULL,
    delivered_at INTEGER
);
CREATE INDEX idx_notification_outbox_pending
    ON notification_outbox(id) WHERE delivered_at IS NULL;
