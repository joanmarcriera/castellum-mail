-- Large operation payloads (sent_copy RFC822 bytes today; attachment-sized
-- payloads later) live here instead of inside operations.payload_json, so
-- listing pending operations stays cheap on every sync/replay/outbox refresh.
-- The payload row keeps only small routing fields; executors fetch the blob
-- by operation id at apply time. Rows cascade with their operation and are
-- deleted early when the operation reaches a terminal succeeded/cancelled
-- state (failed_permanent keeps its blob so a future manual retry can work).
CREATE TABLE IF NOT EXISTS operation_blobs (
    operation_id INTEGER PRIMARY KEY REFERENCES operations(id) ON DELETE CASCADE,
    bytes BLOB NOT NULL
);
