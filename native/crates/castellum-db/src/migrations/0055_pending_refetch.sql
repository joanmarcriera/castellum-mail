-- 0055 (bug hunt #69): remember messages whose queued archive/delete (a `move`) or Empty Trash
-- the user CANCELLED. Both delete the local row up front; the server copy is untouched, and
-- incremental sync never re-imports a UID below UIDNEXT, so the message stayed invisible until
-- a UIDVALIDITY reset. The next sync pass of that mailbox re-fetches exactly these UIDs through
-- the normal descriptor path (not rules-eligible) and clears the rows. A row whose
-- uid_validity no longer matches the mailbox's is dropped unread: that UID names another
-- message now. Keyed by mailbox PATH (the op payload carries paths, not mailbox ids).
CREATE TABLE pending_refetch (
    account_id   TEXT    NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_path TEXT    NOT NULL,
    uid          INTEGER NOT NULL,
    uid_validity INTEGER NOT NULL,
    created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (account_id, mailbox_path, uid_validity, uid)
);
