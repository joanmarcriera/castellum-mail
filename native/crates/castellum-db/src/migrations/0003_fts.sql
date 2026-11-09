-- Full-text search over messages (slice 5). Standalone FTS5 table keyed by the
-- message id (UNINDEXED so it's stored/retrievable but not tokenized). Populated
-- transactionally by upsert_message; rebuildable from `messages`/`message_bodies`.
CREATE VIRTUAL TABLE message_fts USING fts5(
    message_id UNINDEXED,
    subject,
    sender,
    body,
    tokenize = 'unicode61'
);

-- FK cascades don't reach virtual tables, so propagate message deletes (e.g.
-- delete_messages_for_mailbox on a UIDVALIDITY reset) into the FTS index here.
CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
    DELETE FROM message_fts WHERE message_id = OLD.id;
END;
