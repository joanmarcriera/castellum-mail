/*
 * Castellum native core - C ABI.
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2026 Joan Marc Riera Duocastella
 *
 * Hand-written header (slice 1). Keep in sync with
 * native/crates/castellum-ffi/src/lib.rs. A later slice may switch to cbindgen
 * once the exported surface grows richer types.
 *
 * Ownership: strings returned by castellum_version() are allocated by Rust and
 * MUST be released with castellum_string_free(). Paths passed in are borrowed
 * (NUL-terminated UTF-8) and are never retained past the call.
 */
#ifndef CASTELLUM_H
#define CASTELLUM_H
#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes. Values are part of the stable ABI (see CastellumStatus in Rust). */
typedef enum CastellumStatus {
  CASTELLUM_OK = 0,
  CASTELLUM_INVALID_ARGUMENT = 1,
  CASTELLUM_INVALID_HANDLE = 2,
  CASTELLUM_IO = 3,
  CASTELLUM_ALREADY_OPEN = 4,
  CASTELLUM_NETWORK = 5,
  CASTELLUM_INTERNAL = 99
} CastellumStatus;

/* Returns a newly allocated, NUL-terminated UTF-8 version string, or NULL on
 * allocation failure. Free with castellum_string_free(). */
char *castellum_version(void);

/* Markdown compose preview (S46, migration 0025). Stateless, mirrors
 * castellum_version: no profile handle. Renders Markdown to the SAME
 * sanitized HTML that ends up as a draft's text/html MIME alternative when
 * Markdown is on (Draft.body_html) — the live preview and the sent HTML are
 * always the same render. Returns a newly allocated string, free with
 * castellum_string_free; never null. */
char *castellum_render_markdown_preview(const char *markdown);
/* MM-5: as castellum_render_markdown_preview, in a theme: "plain" (the S46
 * output), "clean", "serif" or "compact"; null/unknown = plain. Themes add fixed
 * inline styles after sanitising. Free the result with castellum_string_free. */
char *castellum_render_markdown_themed(const char *markdown, const char *theme);

/* Frees a string previously returned by this library. NULL is accepted. */
void castellum_string_free(char *s);
/* The last error message on the calling thread (a human-readable reason behind a
 * non-OK status), or NULL. Read right after a failed call ON THE SAME THREAD.
 * Free the result with castellum_string_free. */
char *castellum_last_error(void);

/* Opens (creating if needed) the profile at path_utf8; writes a non-zero handle
 * to *out_handle. Returns a CastellumStatus code. */
int32_t castellum_open_profile(const char *path_utf8, uint64_t *out_handle);

/* Cheaply reports how many migrations are pending for the profile at
 * path_utf8, WITHOUT opening it for real or running any of them. Writes the
 * count to *out_pending_count; when non-NULL, out_has_heavy gets whether any
 * pending migration is heavy (0/1), and out_applied_version gets the applied
 * schema watermark observed (0 for a profile that doesn't exist yet or was
 * never migrated -- callers must treat applied_version == 0 as "nothing to
 * protect, no window needed" even though pending_count is also nonzero
 * there). No handle is created. Returns a CastellumStatus code; a profile
 * whose applied schema is newer than this build supports returns CASTELLUM_IO
 * with the classified message set via castellum_last_error, the same
 * convention castellum_open_profile uses for that refusal. */
int32_t castellum_pending_migration_count(const char *path_utf8, int64_t *out_pending_count,
                                           uint8_t *out_has_heavy, int64_t *out_applied_version);

/* Writes the applied DB schema version for handle to *out_version. Returns a
 * CastellumStatus code. */
int32_t castellum_profile_schema_version(uint64_t handle, int64_t *out_version);

/* Returns the directory this profile's log files live in (newly allocated
 * UTF-8 string, free with castellum_string_free), or NULL for an invalid
 * handle. Diagnostics helper only — the directory may be empty if nothing
 * has logged at INFO+ yet; that is not an error. */
char *castellum_log_directory(uint64_t handle);

/* Syncs an IMAP account into the profile. config_json is a UTF-8 JSON object;
 * on success writes a newly allocated JSON report to *out_json (free with
 * castellum_string_free). Read-only sync. Returns a CastellumStatus code. */
/* config_json may carry "imap_security": "tls" | "starttls" | "none" (wins
 * over use_tls; STARTTLS is mandatory when chosen, never a cleartext
 * fallback). Applies to every call taking the stored-account shape. */
int32_t castellum_sync_account(uint64_t handle, const char *config_json, char **out_json);
/* Best-effort snapshot of account_id's CURRENT (or most recently finished)
 * sync pass -- mailbox/message counts, for a non-blocking progress UI to poll
 * alongside a long-running castellum_sync_account call on another thread.
 * Writes a JSON SyncProgress object, or JSON null when no sync has run for
 * this account yet this process -- never an error for that case. Local-only;
 * no network I/O. */
int32_t castellum_sync_progress(uint64_t handle, const char *account_id, char **out_json);
/* Persists an account's identity (id/email/optional display_name, parsed from
 * the same config_json shape castellum_sync_account takes; a blank
 * display_name stores none) WITHOUT syncing -- the "Save anyway"
 * action when the connection check failed. Makes the account visible to
 * castellum_list_accounts; never performs network I/O. Returns a
 * CastellumStatus code. */
int32_t castellum_save_account_without_sync(uint64_t handle, const char *config_json);
/* Account-setup preflight: verifies IMAP (connect, LOGIN, LIST) and, when
 * config_json has an "smtp" object, SMTP (connect, AUTH) WITHOUT persisting
 * anything. Writes a JSON ConnectionTestReport {imap, smtp?, mailboxes} to
 * *out_json (free with castellum_string_free); connection failures are
 * reported inside it as redacted error kinds, so only a malformed request
 * returns a non-OK status. Blocking network I/O -- call off the main thread. */
int32_t castellum_test_connection(uint64_t handle, const char *config_json, char **out_json);
/* Account-setup auto-detect (MM-14). Looks up IMAP/SMTP settings for a mail
 * DOMAIN: DNS SRV (RFC 6186), MX provider hint, the domain's own autoconfig
 * over HTTPS, then Mozilla ISPDB. request_json is
 * {"domain": "...", "sources"?: ["srv","mx","domain_autoconfig","well_known",
 * "ispdb"], "timeout_ms"?: n}; any other field (so the address's local part)
 * is refused. Writes a JSON AutoconfigReport {status, source?, preset_id?,
 * unsupported?, display_name?, imap?, smtp?, username?, host_outside_domain,
 * tried} to *out_json
 * (free with castellum_string_free). A failed or empty lookup is reported
 * inside it (status "not_found"/"offline"); only a malformed request returns
 * a non-OK status. Needs no open profile, persists nothing, never contacts
 * the mail servers it returns, never returns plaintext IMAP. Blocking network
 * I/O bounded by the deadline (default 5 s) -- call off the main thread.
 * host_outside_domain=true means a DNS/MX-derived server is not under the
 * domain: the caller must not pre-fill it. Cancellation is Swift-side only
 * (drop the late result); the call itself ends by its deadline. */
int32_t castellum_autoconfig_lookup(const char *request_json, char **out_json);
/* Writes the last-run sync KPI snapshot for handle to *out_json (free with
 * castellum_string_free): per-phase timings, message count, timestamp and
 * account id from the most recent castellum_sync_account call. *out_json is
 * the JSON string "null" (not a failure status) when no sync has completed
 * in this profile yet. Local-only, no network I/O. Returns a CastellumStatus
 * code. */
int32_t castellum_last_sync_kpi(uint64_t handle, char **out_json);
/* Hydrates one descriptor-only message body from IMAP by exact UID and writes
 * the refreshed MessageDetail (or JSON null when the message is missing).
 * Unlike castellum_get_message, this may perform blocking network I/O. */
int32_t castellum_hydrate_message_body(uint64_t handle, const char *config_json,
                                       const char *message_id, char **out_json);
/* Runs one bounded background body-hydration pass over the account's durable
 * missing-body queue and writes a JSON HydrationReport to *out_json. An empty
 * queue returns without network I/O; callers re-invoke while the report shows
 * progress and remaining backlog. */
int32_t castellum_hydrate_pending_bodies(uint64_t handle, const char *config_json,
                                         char **out_json);
/* Records the user's Accept/Decline/Tentative response (S55) to message_id's
 * calendar invite and queues an iTIP REPLY to its organizer through the
 * normal outbox (still needs draining, same as any other queued send).
 * response must be "accepted", "declined" or "tentative". Writes
 * {"draft_id": i64} to *out_json on success. Local-only. Fails with a
 * "recipient mismatch: <address>" message when the ORGANIZER matches neither
 * the message's From nor Reply-To, unless confirmed_recipient is that exact
 * address. confirmed_recipient is NULL when nothing was confirmed, else the
 * address the confirmation prompt named; a non-NULL value that differs from
 * the address the reply would go to fails with the same message naming the
 * current address (the confirmation is bound to what the user saw). An
 * ORGANIZER that is not one plain addr-spec is always refused. */
int32_t castellum_respond_to_calendar_invite(uint64_t handle, const char *message_id,
                                             const char *response,
                                             const char *confirmed_recipient,
                                             char **out_json);
/* Local-only: is this attachment already cached, and where (JSON
 * AttachmentDownloadResult). Never opens a network connection. */
int32_t castellum_attachment_cache_status(uint64_t handle, const char *message_id,
                                          int64_t ordinal, char **out_json);
/* Downloads one incoming attachment part over IMAP and caches it locally,
 * writing a JSON AttachmentDownloadResult to *out_json. Blocking network I/O
 * unless already cached. */
int32_t castellum_download_attachment(uint64_t handle, const char *config_json,
                                      const char *message_id, int64_t ordinal,
                                      char **out_json);

/* Lists a draft's attachment metadata (no bytes) as JSON. */
int32_t castellum_list_draft_attachments(uint64_t handle, int64_t draft_id, char **out_json);
/* Adds one attachment to a draft from a JSON
 * {filename, content_type, is_inline?, content_id?, data_base64} object;
 * writes the draft's refreshed attachment list to *out_json. */
int32_t castellum_add_draft_attachment(uint64_t handle, int64_t draft_id,
                                       const char *attachment_json, char **out_json);
/* MM-5b: embeds one image in a draft as an inline part. image_json is
 * {filename, alt?, data_base64}. The type is taken from the bytes (PNG, JPEG,
 * GIF or WebP only) and the Content-ID is generated by the core. Writes
 * {"attachment": DraftAttachmentMeta, "markdown": "![alt](cid:...)",
 *  "attachments": [DraftAttachmentMeta]} to *out_json. Returns
 * CASTELLUM_INVALID_ARGUMENT, writing nothing to the profile, when the bytes
 * are not one of those types or exceed 10 MB. */
int32_t castellum_embed_draft_image(uint64_t handle, int64_t draft_id,
                                    const char *image_json, char **out_json);
/* MM-5b: as castellum_render_markdown_themed, but an image whose URL is
 * cid:<Content-ID of an inline attachment of draft_id> renders as an <img>.
 * Every other image is dropped, as before. A draft with no inline attachments
 * renders byte for byte what castellum_render_markdown_themed returns. The
 * compose preview and the sent text/html part both come from here. Writes the
 * HTML to *out_html; free with castellum_string_free. */
int32_t castellum_render_markdown_for_draft(uint64_t handle, int64_t draft_id,
                                            const char *markdown, const char *theme,
                                            char **out_html);
/* MM-5b: local-only. Writes {"content_type", "data_base64"} for the inline
 * attachment of draft_id with this Content-ID (no angle brackets), for the
 * compose preview. The content type is derived from the bytes; anything that
 * is not PNG, JPEG, GIF or WebP returns CASTELLUM_INVALID_ARGUMENT. */
int32_t castellum_draft_inline_image(uint64_t handle, int64_t draft_id,
                                     const char *content_id, char **out_json);
/* MM-5b: makes an existing draft attachment inline (inline_flag != 0) or a
 * regular attachment (0). Writes {"attachments": [...], "markdown": string or
 * null} to *out_json; markdown is the reference to insert when it became
 * inline. Becoming inline needs PNG, JPEG, GIF or WebP bytes within 10 MB
 * (else CASTELLUM_INVALID_ARGUMENT and nothing changes); if the same bytes are
 * already embedded, that row is reused and this duplicate is removed. A draft
 * that has left "draft" status is refused. */
int32_t castellum_set_draft_attachment_inline(uint64_t handle, int64_t draft_id,
                                              int64_t attachment_id, int32_t inline_flag,
                                              char **out_json);
/* Removes one attachment from a draft; writes the refreshed attachment list
 * to *out_json. */
int32_t castellum_remove_draft_attachment(uint64_t handle, int64_t draft_id,
                                          int64_t attachment_id, char **out_json);
/* Copies every attachment of message_id into draft_id (Forward), downloading
 * any not already locally cached. config_json carries the SOURCE message's
 * account credentials. Blocking network I/O. Writes the refreshed attachment
 * list to *out_json. */
int32_t castellum_forward_attachments(uint64_t handle, const char *config_json,
                                      const char *message_id, int64_t draft_id,
                                      char **out_json);
/* Local-only: is this message's raw RFC822 source already cached, and
 * where (JSON {status, local_path, error}). Never opens a network
 * connection. */
int32_t castellum_raw_source_cache_status(uint64_t handle, const char *message_id,
                                          char **out_json);
/* Fetches one message's full raw RFC822 source over IMAP and caches it,
 * writing a JSON {status, local_path, error} to *out_json. Blocking
 * network I/O unless already cached. Feeds both "Message Source" and
 * .eml export. */
int32_t castellum_fetch_raw_source(uint64_t handle, const char *config_json,
                                   const char *message_id, char **out_json);
/* Reads, parses and queues a local .eml file at file_path for import into
 * mailbox_id on account_id, writing a JSON {status, error} to *out_json.
 * No network I/O — the import replays via the durable operation queue. */
int32_t castellum_import_eml(uint64_t handle, const char *account_id,
                             const char *mailbox_id, const char *file_path,
                             char **out_json);

/* Generates the allowlisted, redacted support bundle as JSON in *out_json.
 * Strictly read-only: never writes to the database, never opens a network
 * connection, and never includes credentials, message content, folder names,
 * or raw error text. Free with castellum_string_free. */
int32_t castellum_support_bundle(uint64_t handle, char **out_json);
/* Runs the read-only SQLite integrity check (PRAGMA quick_check) and writes
 * {"ok": bool, "issue_count": n, "first_issue": string|null} to *out_json.
 * Never modifies the profile. Free with castellum_string_free. */
int32_t castellum_integrity_check(uint64_t handle, char **out_json);

/* Best-effort redaction of free text (crash reports, unified-log excerpts) for
 * diagnostics export. NOT the allowlist path castellum_support_bundle uses.
 * Needs no open profile. Writes a newly allocated string to *out_text.
 * Free with castellum_string_free. */
int32_t castellum_redact_diagnostic_text(const char *input_utf8, char **out_text);

/* Read/query APIs. Each writes a newly allocated JSON string to *out_json
 * (free with castellum_string_free) and returns a CastellumStatus code. */
/* Rows are {id, email, display_name (string or null), provider}; the UI shows
 * display_name in place of the address when set (S70). */
int32_t castellum_list_accounts(uint64_t handle, char **out_json);
/* Permanently deletes account_id and every local mail record it owns
 * (mailboxes, messages, sync cursors, queued operations, drafts) via
 * ON DELETE CASCADE plus an explicit drafts purge. Does NOT touch the
 * Keychain — the Swift AccountStore removes stored credentials separately.
 * Returns a CastellumStatus code; errors when the account does not exist. */
int32_t castellum_remove_account(uint64_t handle, const char *account_id);
/* Reset an account's local mail state back to empty:
 * mailboxes, messages, bodies, FTS rows, sync cursors and AI artefacts, in
 * one transaction. UNLIKE castellum_remove_account, the account row itself,
 * its Keychain credentials, its write policy and any rules survive — the
 * caller triggers a fresh sync afterwards. castellum_reset_account_blockers
 * writes {"unsent_outbox_drafts": n, "pending_operations": n} to *out_json —
 * read it first to word the confirmation. castellum_reset_account refuses
 * (status Io; reason prefixed "reset blocked:") while either count is
 * nonzero UNLESS force != 0, which discards them too; errors when the
 * account does not exist. */
int32_t castellum_reset_account_blockers(uint64_t handle, const char *account_id, char **out_json);
int32_t castellum_reset_account(uint64_t handle, const char *account_id, int32_t force);
int32_t castellum_list_mailboxes(uint64_t handle, const char *account_id, char **out_json);
int32_t castellum_mailbox_counts(uint64_t handle, const char *account_id, char **out_json);
/* Per-mailbox body-hydration progress (#3631 G5): JSON array of
 * {"mailbox_id","total","hydrated"}, one row per mailbox of account_id (an
 * empty mailbox is 0/0). total = stored messages with a provider UID;
 * hydrated = those with a local body row. Hydration is budgeted, so
 * hydrated < total is normal on a fresh large account. */
int32_t castellum_body_hydration_progress(uint64_t handle, const char *account_id, char **out_json);
/* Body-hydration coverage of a SEARCH SCOPE (#3633 G13): {"total","missing"}
 * over stored messages with a provider UID; missing = no local body yet, so
 * a body match in them cannot be found. account_id and mailbox_id are each
 * optional (NULL = not narrowed; both NULL = every account); a non-NULL
 * pointer that is not valid UTF-8 returns CASTELLUM_INVALID_ARGUMENT. query
 * (optional) is the search text: only its in:/folder:/path: filters narrow the
 * count, so a search excluding Trash is not told about Trash bodies. Cheap COUNT on
 * idx_messages_uid; safe to call per search. */
int32_t castellum_body_hydration_scope(uint64_t handle, const char *account_id,
                                       const char *mailbox_id, const char *query,
                                       char **out_json);
/* What an unforced castellum_apply_mailbox_selection would refuse on, as a
 * JSON array of {"path": ..., "pending_operations": n} in *out_json (the tracker
 * #2617 follow-up review) — the pre-check the confirmation dialog reads. */
int32_t castellum_apply_mailbox_selection_blockers(uint64_t handle, const char *account_id,
                                                   const char *keep_paths_json, char **out_json);
/* Apply an edited folder selection to an ALREADY-ADDED account (the tracker
 * #2617, Settings > Accounts > Folders… — never the S62 first-sync picker).
 * keep_paths_json is a JSON array of the paths still selected and must not
 * be empty; every other locally known mailbox for account_id is removed
 * right away (messages cascade) UNLESS a non-terminal operation still
 * touches it, in which case the WHOLE call is refused (status Io, reason
 * prefixed "mailbox removal blocked:") and nothing changes — read
 * castellum_apply_mailbox_selection_blockers first, then pass force != 0 only
 * on explicit user confirmation, which cancels those operations with a
 * truthful reason instead. Never touches the server or the Keychain. Writes
 * {"removed": n} to *out_json. */
int32_t castellum_apply_mailbox_selection(uint64_t handle, const char *account_id,
                                          const char *keep_paths_json, int32_t force,
                                          char **out_json);
int32_t castellum_list_messages(uint64_t handle, const char *account_id,
                                const char *mailbox_id, int64_t limit, int64_t offset,
                                char **out_json);

/* Keyset message-list paging (S64). cursor_json is null for the first page or
 * the "next" object from the previous page; writes {"rows":[MessageHeader…],
 * "next":{"sent_at":…,"id":…}|null}. Same ordering and snooze exclusion as
 * castellum_list_messages; cost is independent of how deep the page is.
 * castellum_list_messages_by_role_page is the same shape scoped by mailbox
 * role across every account (the unified-inbox list). */
int32_t castellum_list_messages_page(uint64_t handle, const char *account_id,
                                     const char *mailbox_id, const char *cursor_json,
                                     int64_t limit, char **out_json);
int32_t castellum_list_messages_by_role_page(uint64_t handle, const char *role,
                                             const char *cursor_json, int64_t limit,
                                             char **out_json);

/* Sorted keyset paging (list-sort). sort_key is "date", "from", "subject",
 * "unread", "starred" or "attachments"; NULL or "" means "date". ascending is
 * 0 or 1; with "date", 0 is newest first (the order of
 * castellum_list_messages_page). The last three group by a 0/1 value (ascending
 * puts unread, unflagged, attachment-less rows first) and keep newest first
 * inside each group; their cursor "key" is "0" or "1". Rows without a date are
 * always last. cursor_json is NULL for the first page or the "next" object of
 * the previous page FOR THE SAME sort_key and ascending; a cursor from another
 * sort returns CASTELLUM_INVALID_ARGUMENT, as does an unknown sort_key. Writes
 * {"rows":[MessageHeader…],"next":{"sent_at":…,"id":…,"key":…}|null}; "key" is
 * absent for "date". */
int32_t castellum_list_messages_page_sorted(uint64_t handle, const char *account_id,
                                            const char *mailbox_id, const char *sort_key,
                                            int32_t ascending, const char *cursor_json,
                                            int64_t limit, char **out_json);
int32_t castellum_list_messages_by_role_page_sorted(uint64_t handle, const char *role,
                                                    const char *sort_key, int32_t ascending,
                                                    const char *cursor_json, int64_t limit,
                                                    char **out_json);

int32_t castellum_get_message(uint64_t handle, const char *message_id, char **out_json);

/* RFC 8058 one-click unsubscribe (S44). Performs the POST for a message
 * whose stored List-Unsubscribe targets mark it one-click capable; writes
 * {"status":"sent","http_status":n} or {"status":"unavailable"} to
 * *out_json, or fails with CASTELLUM_NETWORK on any transport/non-2xx
 * error. The caller MUST already have shown the user the exact destination
 * and gotten explicit confirmation before calling this — it performs no
 * confirmation itself. */
int32_t castellum_unsubscribe_one_click(uint64_t handle, const char *message_id, char **out_json);

/* query accepts the S39 search grammar (from:/to:/cc:/subject:/
 * has:attachment/before:/after:/in:/is:, "-" negation, quoted phrases) as
 * well as bare keywords, in both castellum_search_messages and
 * castellum_search_messages_across_accounts below — unchanged signatures. */
int32_t castellum_search_messages(uint64_t handle, const char *account_id,
                                  const char *query, int64_t limit, char **out_json);
int32_t castellum_list_threads(uint64_t handle, const char *account_id,
                               int64_t limit, int64_t offset, char **out_json);
int32_t castellum_get_thread(uint64_t handle, const char *thread_id, char **out_json);

/* Unified (cross-account) reads (S34). castellum_list_messages_by_role lists
 * message headers across every account whose mailbox has `role` (e.g.
 * "inbox"), newest first, paginated exactly like castellum_list_messages.
 * castellum_list_threads_across_accounts is the same drop-the-account-filter
 * shape for castellum_list_threads (threads carry no mailbox role to filter
 * on — not yet wired to a view). castellum_search_messages_across_accounts is
 * castellum_search_messages without the account_id filter. */
int32_t castellum_list_messages_by_role(uint64_t handle, const char *role,
                                        int64_t limit, int64_t offset, char **out_json);
int32_t castellum_list_threads_across_accounts(uint64_t handle, int64_t limit,
                                               int64_t offset, char **out_json);
int32_t castellum_search_messages_across_accounts(uint64_t handle, const char *query,
                                                  int64_t limit, char **out_json);

/* Write path. castellum_mark_read is the EXPLICIT Toggle Read/Unread: it
 * applies the flag locally and enqueues a replayable operation
 * (is_read != 0 = read), and is refused in evaluation mode like every other
 * write. castellum_mark_read_auto is the auto-mark-on-open side effect: never
 * refused, local-only while the account is read-only.
 * castellum_pending_operations writes the queued/running ops for account_id as
 * a JSON array to *out_json. */
int32_t castellum_mark_read(uint64_t handle, const char *message_id, int32_t is_read);
int32_t castellum_mark_read_auto(uint64_t handle, const char *message_id, int32_t is_read);
/* Marks every unread message in a mailbox read (one bulk replay op). */
int32_t castellum_mark_all_read(uint64_t handle, const char *account_id,
                                const char *mailbox_id);
/* Destructive / flag ops (each applies locally + enqueues a replayable op).
 * set_flagged toggles \Flagged; archive/delete move to the Archive/Trash
 * special-use mailbox; empty_trash permanently expunges the Trash. */
int32_t castellum_set_flagged(uint64_t handle, const char *message_id, int32_t flagged);
int32_t castellum_archive_message(uint64_t handle, const char *message_id);
int32_t castellum_delete_message(uint64_t handle, const char *message_id);
int32_t castellum_empty_trash(uint64_t handle, const char *account_id);
/* castellum_move_messages: archive, trash or move a batch. request_json =
 * {"message_ids":["…"],"target":"archive"|"trash"|"mailbox","mailbox_id":"…"}
 * (mailbox_id only for "mailbox"). Each message is its own transaction; one
 * refusal does not stop the rest. *out_json =
 * {"operation_ids":[int64…],
 *  "refused":[{"message_id":"…","reason":"…","evaluation_mode":bool}]}.
 * operation_ids are opaque undo handles for castellum_undo_moves. */
int32_t castellum_move_messages(uint64_t handle, const char *request_json, char **out_json);
/* castellum_undo_moves: queue the inverse of earlier moves. request_json =
 * {"operation_ids":[int64…]}. Atomic for the group. *out_json =
 * {"queued":n,"account_ids":["…"],"source_mailbox_ids":["…"],
 *  "not_undoable":[{"operation_id":int64,"reason":"…","evaluation_mode":bool}]}.
 * The messages return on the next replay + sync, as new rows. */
int32_t castellum_undo_moves(uint64_t handle, const char *request_json, char **out_json);
/* castellum_delete_message_permanently: DESTRUCTIVE. Queues a UID EXPUNGE of
 * ONE message that is already in Trash; refused (non-zero status) for any
 * message outside Trash. The local row stays until the server confirms, so a
 * failed expunge leaves the message visible. Callers must show an explicit
 * "cannot be undone" confirmation first. */
int32_t castellum_delete_message_permanently(uint64_t handle, const char *message_id);

/* Snooze (S43, migration 0024). castellum_snooze_message snoozes (or
 * re-snoozes) a message until until_epoch (Unix epoch seconds); errors if
 * the message doesn't exist. castellum_unsnooze_message un-snoozes it now,
 * writing {"unsnoozed": bool}. castellum_list_snoozed_messages lists every
 * currently-snoozed message, soonest due first, as a JSON array of message
 * headers each carrying snoozed_until_epoch. castellum_unsnooze_due pops
 * every snooze due at or before now_epoch, writing a JSON array of
 * {"message_id":..., "account_id":...}; call on launch and on a periodic
 * sweep — SQLite is the only source of truth, so this is safe to call
 * repeatedly and after any crash. */
int32_t castellum_snooze_message(uint64_t handle, const char *message_id, int64_t until_epoch);
int32_t castellum_unsnooze_message(uint64_t handle, const char *message_id, char **out_json);
int32_t castellum_list_snoozed_messages(uint64_t handle, int64_t limit, int64_t offset,
                                        char **out_json);
int32_t castellum_unsnooze_due(uint64_t handle, int64_t now_epoch, char **out_json);

/* Tags and mute (S42, migration 0041). Tags are IMAP keywords; definitions
 * (display name, colour, order) are local. castellum_set_message_tag writes
 * {"synced": bool, "reason": null|"evaluation_mode"} (a mailbox
 * refusing the keyword is found at replay and demoted silently, not reported).
 * castellum_list_tags writes an array of TagDefinition; castellum_upsert_tag
 * takes/returns one (empty keyword = derive from display_name). Mute is local
 * and conversation-scoped (all messages sharing the thread). */
int32_t castellum_list_tags(uint64_t handle, char **out_json);
int32_t castellum_upsert_tag(uint64_t handle, const char *tag_json, char **out_json);
int32_t castellum_delete_tag(uint64_t handle, const char *keyword);
int32_t castellum_set_message_tag(uint64_t handle, const char *message_id, const char *keyword,
                                  int32_t add, char **out_json);
int32_t castellum_set_thread_muted(uint64_t handle, const char *message_id, int32_t muted);
int32_t castellum_list_keywords_in_use(uint64_t handle, char **out_json);
int32_t castellum_tag_counts(uint64_t handle, char **out_json);

/* MM-8. castellum_tag_counts entries gain "local_only" (messages whose tag
 * never reached a server). Counts and castellum_messages_for_tag share one
 * rule: every account and folder except Trash and Junk.
 * castellum_messages_for_tag writes a JSON array of MessageHeader, newest
 * first, keyset-paged: after_id NULL = first page, else the id and sent_at
 * (0 when the message has none) of the previous page's last row. An unknown
 * keyword writes []. Hides snoozed mail and collapses one email synced into
 * several folders to one row. Local-only, no network I/O. */
int32_t castellum_messages_for_tag(uint64_t handle, const char *keyword, int64_t limit,
                                   int64_t after_sent_at, const char *after_id,
                                   char **out_json);

/* Notifications and VIPs (S59, migration 0048). castellum_take_pending_notifications
 * pops up to `limit` undelivered new-mail notices (JSON array of PendingNotification)
 * and marks them delivered (at-most-once). castellum_set_vip writes {"changed": bool};
 * display_name may be NULL. castellum_vip_unread_count takes a JSON array of account
 * ids or NULL (= all accounts). */
int32_t castellum_set_vip(uint64_t handle, const char *address, const char *display_name,
                          int32_t vip, char **out_json);
int32_t castellum_list_vips(uint64_t handle, char **out_json);
int32_t castellum_take_pending_notifications(uint64_t handle, int64_t limit, char **out_json);
int32_t castellum_vip_unread_count(uint64_t handle, const char *account_ids_json,
                                   int64_t *out_count);

/* Per-account write policy (fail-closed evaluation mode). New accounts default
 * to read-only (0). set_account_write_enabled with enabled != 0 leaves
 * evaluation mode (explicit human act). account_write_enabled writes 1/0 to
 * out_enabled; an unknown account reads 0. */
int32_t castellum_set_account_write_enabled(uint64_t handle, const char *account_id,
                                            int32_t enabled);
int32_t castellum_account_write_enabled(uint64_t handle, const char *account_id,
                                        int32_t *out_enabled);
int32_t castellum_pending_operations(uint64_t handle, const char *account_id, char **out_json);
/* Manual outbox recovery. Retry resets a failed/failed_permanent op to
 * queued (fresh attempt budget; blob kept); cancel makes a queued/failed/
 * failed_permanent op terminal (blob reclaimed). Running ops and terminal
 * ops are benign no-ops; unknown ids error. */
int32_t castellum_retry_operation(uint64_t handle, int64_t op_id);
int32_t castellum_cancel_operation(uint64_t handle, int64_t op_id);
/* Replays queued ops to IMAP (config_json carries account_id + credentials);
 * writes a JSON ReplayReport to *out_json. */
int32_t castellum_replay_operations(uint64_t handle, const char *config_json, char **out_json);

/* Compose / send. draft_json = {id?, account_id, to, cc?, bcc?, subject?,
 * body_text?, in_reply_to?, references_header?} with to/cc/bcc as arrays of
 * {name?, email}. castellum_save_draft writes the draft id to *out_id.
 * castellum_send_draft queues a draft (status -> outbox + one 'send' op).
 * castellum_send_pending delivers queued drafts over SMTP; config_json =
 * {account_id, from_email, from_name?, plain_text_flowed?, smtp: {host, port,
 * username, password, use_tls, allow_invalid_certs}}; plain_text_flowed (bool,
 * default false) sends a plain-text-only draft as RFC 3676 format=flowed;
 * writes a JSON SendReport to *out_json. */
int32_t castellum_save_draft(uint64_t handle, const char *draft_json, int64_t *out_id);
int32_t castellum_list_drafts(uint64_t handle, const char *account_id, char **out_json);
int32_t castellum_delete_draft(uint64_t handle, int64_t draft_id);
int32_t castellum_send_draft(uint64_t handle, int64_t draft_id);
int32_t castellum_send_pending(uint64_t handle, const char *config_json, char **out_json);
/* Outbox (OB1). castellum_outbox_sends writes every draft in 'outbox' status
 * for account_id as a JSON array of {draft_id, account_id, operation_id?,
 * subject, recipients, status, attempts, error_kind?, automatic_retry,
 * next_attempt_epoch?, scheduled, updated_at_epoch}; status is the send op's
 * status or "stranded" (no live op); error_kind is a kind token only (auth,
 * tls, network, protocol, unsupported, internal), never server text.
 * castellum_send_draft_now attempts exactly draft_id now (config_json as for
 * castellum_send_pending), ignoring backoff and permanent failure; writes a
 * SendReport (blocked = 1 on a read-only account). An authentication refusal
 * is permanent: automatic drains never retry it. castellum_requeue_failed_sends
 * gives every failed send of account_id a fresh attempt ("Send All"); writes
 * {"requeued": n}. */
int32_t castellum_outbox_sends(uint64_t handle, const char *account_id, char **out_json);
int32_t castellum_send_draft_now(uint64_t handle, const char *config_json, int64_t draft_id,
                                 char **out_json);
int32_t castellum_requeue_failed_sends(uint64_t handle, const char *account_id, char **out_json);

/* S36: server drafts. castellum_save_draft_to_server builds RFC822 bytes for
 * draft_id (from_json = {"from_email": str, "from_name": str?}) and enqueues
 * a server_draft_save replay op; writes {"queued": bool} to *out_json (free
 * with castellum_string_free) - false means the draft already left 'draft'
 * status. castellum_open_server_draft hydrates (or reuses) a local editable
 * draft from an already-synced message_id in a Drafts-role mailbox, writing
 * the draft id to *out_id. */
int32_t castellum_save_draft_to_server(uint64_t handle, int64_t draft_id,
                                        const char *from_json, char **out_json);
int32_t castellum_open_server_draft(uint64_t handle, const char *message_id, int64_t *out_id);

/* Undo-send window (migration 0020). castellum_send_draft_delayed queues a
 * draft like castellum_send_draft but with a durable earliest-send time
 * (delay_seconds <= 0 is identical to castellum_send_draft); the sender skips
 * the op until that time passes. castellum_undo_send atomically cancels a
 * not-yet-submitted 'send' op and returns its draft to 'draft' status IF the
 * sender has not already claimed it (same claiming UPDATE as the sender's own
 * begin_operation_attempt, so this can never race a send in progress); writes
 * {"in_time": bool} to *out_json. castellum_send_now releases a 'held' send
 * (crash-safety hold, below) for immediate delivery; writes
 * {"released": bool}. castellum_reconcile_undo_sends is the crash-safety
 * startup pass — call it ONCE per account at launch, before scheduling any
 * drain: it holds any queued 'send' op whose undo window cannot be proven to
 * have stayed open under a continuously-live app (see DECISIONS.md), so a
 * delayed send is NEVER auto-delivered after an ambiguous crash. The same pass
 * holds EVERY send found 'running' (instant, undo-send or scheduled): it was
 * being delivered when the app quit, whether it went out is unknown, and
 * resubmitting it could mail a second copy. Such an op is 'held' with a
 * last_error starting "may have been sent". A held op shows in the outbox and
 * only castellum_send_now / castellum_undo_send move it. Writes {"held": n}. */
int32_t castellum_send_draft_delayed(uint64_t handle, int64_t draft_id, int64_t delay_seconds);
int32_t castellum_undo_send(uint64_t handle, int64_t draft_id, char **out_json);
int32_t castellum_send_now(uint64_t handle, int64_t draft_id, char **out_json);
int32_t castellum_reconcile_undo_sends(uint64_t handle, const char *account_id, char **out_json);

/* Send Later (HP1). castellum_schedule_send queues a draft for delivery at
 * send_at (Unix epoch seconds, UTC, must be in the future) on the same durable
 * 'send' op as the undo-send window, flagged scheduled: it waits through quit
 * and relaunch. castellum_reschedule_send moves a waiting or needs-confirmation
 * scheduled send; writes {"rescheduled": bool}. castellum_cancel_scheduled_send
 * returns the draft, intact, to Drafts; writes {"cancelled": bool}
 * (castellum_send_now also releases a waiting scheduled send). The
 * castellum_pending_operations JSON gains "scheduled": bool and uses
 * not_before_epoch as the scheduled time; extra statuses: "held" with a
 * last_error starting "may have been sent" (in flight at a crash, NEVER retried
 * automatically; any send can be in this state, not only a scheduled one) and
 * "needs_confirmation" (more than ten minutes late at launch, scheduled sends
 * only). Both wait for castellum_send_now or castellum_cancel_scheduled_send. */
int32_t castellum_schedule_send(uint64_t handle, int64_t draft_id, int64_t send_at);
int32_t castellum_reschedule_send(uint64_t handle, int64_t draft_id, int64_t send_at, char **out_json);
int32_t castellum_cancel_scheduled_send(uint64_t handle, int64_t draft_id, char **out_json);

/* Contacts (slice 8a). castellum_refresh_contacts recomputes the contacts
 * aggregate + co-occurrence edges from synced mail and writes a JSON
 * AggregateReport {contacts_upserted, edges_rebuilt, elapsed_ms} to *out_json;
 * user-owned fields (display_name, notes, is_hidden, is_user_created) are
 * never touched by a refresh. castellum_list_contacts takes a NULLABLE query
 * (null/blank = no filter; otherwise substring-matches email/derived_name/
 * display_name) and writes a JSON array. castellum_get_contact writes one
 * contact WITH enrichment rows (JSON null when not found). castellum_update_contact
 * takes {id, display_name?, notes?, is_hidden?} and updates ONLY user-owned fields.
 * castellum_create_contact takes {email, name?} and writes the stored contact
 * (is_user_created = true) to *out_json. */
int32_t castellum_refresh_contacts(uint64_t handle, char **out_json);

/* Stage X1. One-off repair of subjects and display names that an earlier
 * build stored RFC 2047-encoded ("=?UTF-8?Q?...?="): decodes them, re-indexes
 * those rows for search and re-threads. Runs in bounded batches, may be
 * interrupted (the next call resumes), and returns at once after it has
 * completed for the profile. Call it OFF the launch path. Writes a JSON
 * EnvelopeTextRepairReport {already_done, batches, scanned, messages_repaired,
 * subjects_repaired, names_repaired} to *out_json. */
int32_t castellum_repair_encoded_envelope_text(uint64_t handle, char **out_json);
int32_t castellum_list_contacts(uint64_t handle, const char *query,
                                int32_t include_hidden, int64_t limit,
                                char **out_json);
/* Recipients (to/cc/bcc) of sent or queued mail, newest first, at most `limit`,
 * as a JSON array [{email, name?, last_sent_at, sent_count}] -- address
 * autocomplete source that works before the Sent copy has synced back. */
int32_t castellum_list_sent_recipients(uint64_t handle, int64_t limit, char **out_json);
int32_t castellum_get_contact(uint64_t handle, int64_t contact_id, char **out_json);
int32_t castellum_update_contact(uint64_t handle, const char *json);
int32_t castellum_create_contact(uint64_t handle, const char *json, char **out_json);

/* Per-account identities/aliases (S47, migration 0026). The account's own
 * address is never a row here — only the EXTRA From addresses a compose
 * window's From picker offers. castellum_list_identities writes a JSON
 * array to *out_json. castellum_create_identity takes {account_id, email,
 * display_name?} and writes the stored row to *out_json; refuses a
 * malformed email or a duplicate (account_id, email) pair.
 * castellum_update_identity takes {id, display_name?, reply_to?, bcc?, is_default?}:
 * an absent field is left alone; "" clears display name / reply-to / bcc (MM-6).
 * castellum_delete_identity takes a bare id. */
int32_t castellum_list_identities(uint64_t handle, const char *account_id, char **out_json);
int32_t castellum_create_identity(uint64_t handle, const char *json, char **out_json);
int32_t castellum_update_identity(uint64_t handle, const char *json);
int32_t castellum_delete_identity(uint64_t handle, int64_t identity_id);

/* Contact enrichment (slice 8d). JSON-dispatch surface; op_json must be one of:
 *   {"op":"list_sources"} → [{id, display_name, kind, input_label}, ...]
 *   {"op":"enrich","contact_id":N,"source_id":"...","input":"...","save":bool}
 *       → preview (save=false: rows are {field,value,provenance} only) or
 *         save-and-return (save=true: rows carry id/source/fetched_at too)
 *   {"op":"delete","id":N} → "{}" on success
 *   {"op":"raw_add","contact_id":N,"source":"...","field":"...","value":"...",
 *    "provenance":null} → "{}" on success (direct upsert, bypasses the parser)
 * Writes the result JSON to *out_json (free with castellum_string_free). */
int32_t castellum_enrichment(uint64_t handle, const char *op_json, char **out_json);

/* Projects (slice 8b).
 * castellum_list_projects: writes a JSON array of ProjectCard records to *out_json.
 *   include_archived != 0 includes archived projects.
 * castellum_upsert_project: creates (id absent/empty) or updates a project from
 *   {id?, name, notes?, status?, color?, position?} JSON; writes the stored project.
 * castellum_delete_project: permanently deletes the project and its link rows.
 * castellum_get_project: writes a ProjectDetail (project + emails + contacts) as
 *   JSON to *out_json (JSON null when not found).
 * castellum_project_set_email_link: links (linked!=0) or unlinks a message.
 *   auto_contacts!=0 inserts non-self non-hidden participants as auto contacts.
 * castellum_project_set_contact_link: links/unlinks a contact (manual origin);
 *   linking over an 'auto' row upgrades it to 'manual'.
 * castellum_projects_for_message: reverse lookup — writes [{project_id,name,color}]
 *   JSON to *out_json for all projects containing message_id.
 * castellum_move_project: moves a project to a new status column + position,
 *   re-packing both affected columns atomically. */
int32_t castellum_list_projects(uint64_t handle, int32_t include_archived,
                                char **out_json);
int32_t castellum_upsert_project(uint64_t handle, const char *json, char **out_json);
int32_t castellum_delete_project(uint64_t handle, const char *project_id);
int32_t castellum_get_project(uint64_t handle, const char *project_id, char **out_json);
int32_t castellum_project_set_email_link(uint64_t handle, const char *project_id,
                                         const char *message_id, int32_t linked,
                                         int32_t auto_contacts);
int32_t castellum_project_set_contact_link(uint64_t handle, const char *project_id,
                                            int64_t contact_id, int32_t linked);
int32_t castellum_projects_for_message(uint64_t handle, const char *message_id,
                                        char **out_json);
int32_t castellum_move_project(uint64_t handle, const char *project_id,
                               const char *status, int64_t position);

/* Smart mailboxes (S41): named, saved S39 search-grammar queries, run
 * cross-account through the same path castellum_search_messages_across_accounts
 * uses — no second filter language.
 * castellum_list_smart_mailboxes: writes a JSON array of SmartMailbox records.
 * castellum_upsert_smart_mailbox: creates (id absent/empty) or renames/re-queries
 *   (id present) from {id?, name, query?, symbol?, group?, scope?} JSON; recomputes
 *   counts inline. MM-1: `group` is a nested FilterGroup {op: and|or|none,
 *   conditions:[{field,operator,value}], groups:[...]} making it a group mailbox
 *   (query then stored as ""); `scope` is {account_ids:[], mailbox_ids:[]}, empty
 *   lists = everywhere; HP-2 adds "inbox_split": bool (default false). SmartMailbox JSON carries the same `group` (null for a
 *   text-query mailbox) and `scope`. An empty, over-deep (8) or unsupported group
 *   is an error and stores nothing.
 * castellum_delete_smart_mailbox: permanently deletes (never touches mail).
 * castellum_reorder_smart_mailboxes: json = a JSON array of ids in sidebar order.
 * castellum_refresh_smart_mailbox_counts: recomputes every smart mailbox's cached
 *   counts; writes the refreshed list as JSON. Call after a sync pass.
 * castellum_messages_for_smart_mailbox: writes a JSON array of MessageHeader, or
 *   JSON null when smart_mailbox_id is unknown (never an error).
 * MM-1c: a condition's field may be {"header":"List-Id"} (any stored header; operators
 *   contains|equals|starts_with + text, or is + bool = present/absent). Names that are
 *   invalid, have their own field, or are never stored make the upsert fail. */
int32_t castellum_list_smart_mailboxes(uint64_t handle, char **out_json);
int32_t castellum_upsert_smart_mailbox(uint64_t handle, const char *json, char **out_json);
int32_t castellum_delete_smart_mailbox(uint64_t handle, const char *smart_mailbox_id);
int32_t castellum_reorder_smart_mailboxes(uint64_t handle, const char *json);
int32_t castellum_refresh_smart_mailbox_counts(uint64_t handle, char **out_json);
int32_t castellum_messages_for_smart_mailbox(uint64_t handle, const char *smart_mailbox_id,
                                             int64_t limit, char **out_json);

/* MM-1c header conditions. castellum_filter_header_policy writes
 *   {"max_name_len":64,"builtin_fields":{"from":"from",...},"not_stored":[...],
 *    "not_stored_prefixes":[...],"not_stored_substrings":[...],"known_names":[...]}
 *   known_names = up to 200 lower-cased header names seen in this profile, for
 *   completion. Local-only, no network I/O. Free with castellum_string_free.
 * castellum_message_header_coverage writes {"captured":N,"total":M}: messages whose
 *   headers are stored vs all messages. Local-only. */
int32_t castellum_filter_header_policy(uint64_t handle, char **out_json);
int32_t castellum_message_header_coverage(uint64_t handle, char **out_json);

/* Split Inbox (HP-2): a split is a group smart mailbox whose scope has
 * "inbox_split": true (set through castellum_upsert_smart_mailbox). The Inbox is
 * partitioned in split (sidebar) order: each message belongs to the FIRST split
 * it matches, or to Other (id "other") when it matches none, so it is never in
 * two tabs. Snoozed mail is in no tab, as in the Inbox list.
 * castellum_inbox_split_counts: json = {"mailbox_ids": [...]} (empty = every
 *   account's Inbox; non-Inbox ids are ignored); writes a JSON array of
 *   {id, name, total, unread}, one per split then Other (always last).
 * castellum_inbox_split_messages: json = {"mailbox_ids": [...], "split_id": a
 *   split id or "other", "limit"?: n (default 500)}; writes a JSON array of
 *   MessageHeader (newest first), or JSON null when the split no longer exists. */
int32_t castellum_inbox_split_counts(uint64_t handle, const char *json, char **out_json);
int32_t castellum_inbox_split_messages(uint64_t handle, const char *json, char **out_json);

/* Filters / rules (S40): one declarative FilterGroup schema backs both saved
 * smart mailboxes (kind = "smart_mailbox") and rules (kind = "rule", which
 * additionally carry a trigger and a list of actions). Rule actions ALWAYS
 * route through the existing operations queue (0005_operations.sql) via the
 * SAME mutators a user-initiated Move/star/mark-read already uses — never
 * direct provider I/O, never a second write path. Dry-run/preview
 * (castellum_evaluate_filter, castellum_preview_rule) never touches that
 * queue: no call into its insert path exists on those read-only entry
 * points, by construction.
 * castellum_upsert_filter: creates (id absent/empty) or updates (id present)
 *   from {id?, account_id?, name, kind, group, sort_field?, sort_desc?,
 *   trigger?, actions?} JSON; writes the stored row as JSON.
 * castellum_list_filters: account_id NULLABLE (null = every filter, non-null
 *   = that account's own filters plus every account-agnostic one); writes a
 *   JSON array.
 * castellum_get_filter: writes one filter as JSON, or JSON null if unknown.
 * castellum_delete_filter: permanently deletes; errors if filter_id is
 *   unknown.
 * castellum_evaluate_filter: group_json is a serialized FilterGroup (not the
 *   memo's original filter_json_or_id union — split for C-boundary clarity,
 *   see project-docs/plans/2026-09-23-rules-filters-engine.md's Deviations
 *   log); account_id NULLABLE. Writes matching MessageHeader rows as JSON,
 *   same shape as castellum_search_messages. Does NOT run rule actions.
 * castellum_preview_rule: additive extension (not in the memo's original §6
 *   list). Previews a kind = "rule" filter: writes
 *   {"matches":[...MessageHeader...],"actions":[...],"total":N} JSON without
 *   enqueueing anything; matches are the first `limit` candidates and total
 *   is the exact count Run Now would act on (Trash/Junk/Drafts/Sent left out
 *   in SQL, before the limit). account_id NULLABLE. Errors if filter_id is
 *   unknown or not a rule.
 * castellum_preview_rule_conditions (MM-3 review): the same dry run for an
 *   UNSAVED group_json: {"matches":[...],"total":N}. Read-only.
 * castellum_run_rule_now_batch (MM-3 review): one bounded Run Now batch —
 *   queues the rule's actions for its next `limit` (clamped 1..1000)
 *   candidates whose message id sorts after `after` (NULLABLE = start) and
 *   writes one RuleRunReport ({"filter_id","matched_count",
 *   "actions_enqueued","error"?,"next_after"?,"remaining"?}); pass next_after back as
 *   `after` until it is absent. The app loops this off the main thread.
 * castellum_run_rule_now: the one-shot "Run now"/Manual path: runs exactly
 *   one rule regardless of its own trigger_kind against its current scope,
 *   at most 10,000 messages (next_after set when it stopped there).
 *   account_id NULLABLE (falls back to the filter's own stored account_id).
 *   Writes one RuleRunReport as JSON.
 * (castellum_run_rules was removed in the MM-3 review: it ignored rule order,
 *   enabled and stop_processing, and nothing called it. Arrival rules run
 *   only inside a sync pass.) */
int32_t castellum_upsert_filter(uint64_t handle, const char *filter_json, char **out_json);
/* MM-3: run order for rules. json = a JSON array of filter ids in run order;
 * unknown ids are ignored. Rules run in this order after each sync; a rule
 * with stop_processing consumes the messages it matched. castellum_upsert_filter
 * also accepts optional stop_processing / enabled booleans, and the SavedFilter
 * JSON carries sort_order, stop_processing and enabled. RuleAction additionally
 * has "mark_unread", "archive", "trash" and {"remove_flag":{"flag":"starred"}}. */
int32_t castellum_reorder_filters(uint64_t handle, const char *json);
int32_t castellum_list_filters(uint64_t handle, const char *account_id, char **out_json);
int32_t castellum_get_filter(uint64_t handle, const char *filter_id, char **out_json);
int32_t castellum_delete_filter(uint64_t handle, const char *filter_id);
int32_t castellum_evaluate_filter(uint64_t handle, const char *group_json, const char *account_id,
                                  int64_t limit, int64_t offset, char **out_json);
int32_t castellum_preview_rule(uint64_t handle, const char *filter_id, const char *account_id,
                               int64_t limit, char **out_json);
int32_t castellum_preview_rule_conditions(uint64_t handle, const char *group_json,
                                          const char *account_id, int64_t limit, char **out_json);
int32_t castellum_run_rule_now_batch(uint64_t handle, const char *filter_id, const char *account_id,
                                     const char *after, int64_t limit, char **out_json);
int32_t castellum_run_rule_now(uint64_t handle, const char *filter_id, const char *account_id,
                               char **out_json);

/* ── AI job queue (slice 7b) ─────────────────────────────────────────────── */
int32_t castellum_enqueue_ai_job(uint64_t handle, const char *request_json, char **out_json);
int32_t castellum_run_ai_jobs(uint64_t handle, const char *config_json, char **out_json);
/* castellum_poll_ai_job: {"job":{id,status,feature,attempts,error_code,
 * duration_ms,tokens_in,tokens_out,tokens_per_second},"events":[...]}. The last
 * four job fields are additive latency telemetry (M5): wall milliseconds of the
 * generate/embed call and tokens/s where the backend reported counts; JSON null
 * until the job completes (and always for older jobs). Counts and timings only. */
int32_t castellum_poll_ai_job(uint64_t handle, int64_t job_id, int64_t after_event_id, char **out_json);
int32_t castellum_cancel_ai_job(uint64_t handle, int64_t job_id);
int32_t castellum_ai_list_models(uint64_t handle, const char *config_json, char **out_json);
int32_t castellum_get_ai_output(uint64_t handle, const char *scope, const char *owner_id, const char *feature, char **out_json);
/* castellum_get_thread_extractions: S54's extract_thread_items typed rows for
 * a thread. Writes a JSON array to *out_json — [] when nothing has been
 * extracted yet (never an error for that). */
int32_t castellum_get_thread_extractions(uint64_t handle, const char *thread_id, char **out_json);

/* "Find similar" (S53-UI). Embeds message_id (ad-hoc, reusing its stored
 * embedding when one already exists — the common case, since S52 embeds
 * every synced message on ingest), then finds its nearest neighbours by
 * cosine similarity over message_embeddings. config_json is the same shape
 * castellum_run_ai_jobs/castellum_ai_list_models take (endpoint, model,
 * optional embedding_model). account_id is NULLABLE (null = every account).
 * Writes {"results":[{"header":MessageHeader,"similarity":f32}, ...]} to
 * *out_json, most similar first. This is NOT part of the ordinary search
 * path above (castellum_search_messages/castellum_search_messages_across_accounts,
 * whose signatures this stage does not touch) — it is invoked only from an
 * explicit "Find similar" gesture, never on the typing path, per
 * DECISION-S53-ui. MAY TAKE LONGER than other search calls: the ad-hoc-embed
 * path is a network round trip to Ollama. The caller MUST invoke this off
 * the main thread. */
int32_t castellum_find_similar_messages(uint64_t handle, const char *config_json,
                                        const char *message_id, const char *account_id,
                                        int64_t limit, char **out_json);

/* Local-only diagnostics (never export to users or include in support bundles).
 * Writes {"accounts":[{account_id, email, provider, last_attempt_at?,
 * last_success_at?, last_error_kind?, last_error?, last_error_phase?,
 * consecutive_failures, mailboxes:[{mailbox_id, path, role?, ...}]}]} to
 * *out_json. Identity fields (email, path) are REAL — never anonymised here.
 * Error values are still re-validated redacted phrases. Free with
 * castellum_string_free. MUST NEVER be written to the exported support bundle. */
int32_t castellum_local_diagnostics(uint64_t handle, char **out_json);

/* ── People network graph (slice 8c) ────────────────────────────────────── */
/* castellum_contact_graph: writes {"nodes":[{id,label,message_count}],
 * "edges":[{a,b,weight}]} JSON to *out_json.
 * project_id is NULLABLE: null = all contacts (global graph); non-null restricts
 * both nodes and edges to project members (both endpoints must be in the project).
 * max_nodes is clamped to [1..200]. Returns a CastellumStatus code. */
int32_t castellum_contact_graph(uint64_t handle,
                                const char *project_id,
                                int64_t max_nodes,
                                char **out_json);

/* ── Shared evidence-backed link model (migration 0017) ────────────────── */
/* castellum_upsert_link: request_json = {src_kind, src_id, dst_kind, dst_id,
 * relation, origin?, provenance?, confidence?} (origin defaults to 'manual';
 * kinds: project|message|thread|contact|work_item). Writes the stored
 * LinkRecord JSON to *out_json: {id, src_kind, src_id, dst_kind, dst_id,
 * relation, origin, provenance?, confidence?, note?, created_at, updated_at}.
 * Idempotent; manual rows are never downgraded and derived refreshes never
 * touch the user note. Free with castellum_string_free. */
int32_t castellum_upsert_link(uint64_t handle, const char *request_json, char **out_json);

/* castellum_unlink: request_json = {src_kind, src_id, dst_kind, dst_id,
 * relation}. Idempotent (missing link is a success). */
int32_t castellum_unlink(uint64_t handle, const char *request_json);

/* castellum_set_link_note: request_json = {src_kind, src_id, dst_kind,
 * dst_id, relation, note?} (note null/absent clears). The note is user-owned:
 * derived refreshes can never overwrite it. Errors when the link is missing. */
int32_t castellum_set_link_note(uint64_t handle, const char *request_json);

/* castellum_links_for: every link touching (kind, id) in either direction,
 * newest activity first, as a JSON array of LinkRecord (see
 * castellum_upsert_link). The provenance field answers "why is this
 * connected?" using entity ids only. Free with castellum_string_free. */
int32_t castellum_links_for(uint64_t handle, const char *kind, const char *id, char **out_json);

/* ── Work items (Active Work backend, migration 0018) ──────────────────── */
/* castellum_upsert_work_item: request_json = {id?, project_id?, title, notes?,
 * state?, position?, due_at?, delegated_to_contact_id?}; state in
 * needs_reply|waiting|delegated|blocked|done|someday (default needs_reply).
 * Writes the stored WorkItem JSON to *out_json:
 * {id, project_id (null = unassigned), title, notes, state, position,
 *  created_at, updated_at, due_at, waiting_since, completed_at,
 *  delegated_to_contact_id} (the last four are null when unset; times are
 * epoch seconds). waiting_since is set on entering waiting and cleared on
 * leaving it; completed_at likewise for done; delegated_to_contact_id is only
 * valid in state delegated. Omitted due_at / delegated_to_contact_id keep the
 * stored value on update, an explicit null clears it. Omitted notes likewise
 * keep the stored notes on update (empty on create); an explicit "" clears
 * them. Old requests (without the new keys) keep working. Title/notes are
 * user-owned; work items are never auto-created from mail. Free with
 * castellum_string_free. */
int32_t castellum_upsert_work_item(uint64_t handle, const char *request_json, char **out_json);

/* castellum_list_work_items: the project's work items in deterministic order
 * (state lane, position, created_at, id) as a JSON array of WorkItem.
 * Free with castellum_string_free. */
int32_t castellum_list_work_items(uint64_t handle, const char *project_id, char **out_json);

/* castellum_list_work_items_filtered: filter_json = {project_id?, states?
 * (array), due_before?, due_after? (epoch seconds), overdue? (due earlier than
 * now and state not done/someday), contact_id? (integer; delegated to or
 * linked to that contact), linked_message_id?, linked_thread_id?,
 * unassigned? (no project), due? (array of "today"|"this_week"|"overdue",
 * OR-ed, relative to the local clock), waiting_on_me? (true = needs_reply),
 * waiting_on_others? (true = waiting or delegated)}. All keys combine with AND; an unknown key is an
 * error. Writes a JSON array of WorkItem ordered by due_at ascending (undated
 * last), then position, then id. Free with castellum_string_free. */
int32_t castellum_list_work_items_filtered(uint64_t handle, const char *filter_json, char **out_json);

/* castellum_work_items_for: request_json = {"kind":"message"|"thread"|
 * "contact","id":"..."}. Writes a JSON array of the work items linked to that
 * endpoint (a contact also matches items delegated to it), same order as the
 * filtered list. Free with castellum_string_free. */
int32_t castellum_work_items_for(uint64_t handle, const char *request_json, char **out_json);

/* castellum_list_suggestions: suggestions you accept (AW2) as a JSON array of
 * Suggestion {id, kind, status, payload, message_id?, thread_id?,
 * extraction_id?, rfc822_message_id?, target_work_item_id?,
 * created_work_item_id?, origin, confidence?, dedupe_key, created_at,
 * decided_at?}, newest first. filter_json = {status?
 * (proposed|accepted|dismissed), kind? (work_item|waiting_for_reply|
 * state_change), thread_id?, project_id?, limit?}; NULL or "" lists all; an
 * unknown key is an error. Evidence fields are ids only. Free with
 * castellum_string_free. */
int32_t castellum_list_suggestions(uint64_t handle, const char *filter_json, char **out_json);

/* castellum_accept_suggestion: creates the work item (or applies the state
 * change) and links it to the evidence message and thread, in one
 * transaction. Idempotent. Writes {suggestion, work_item}. Accepting a
 * dismissed suggestion is an error. Free with castellum_string_free. */
int32_t castellum_accept_suggestion(uint64_t handle, const char *id, char **out_json);

/* castellum_dismiss_suggestion: idempotent; the suggestion's dedupe key is
 * never proposed again. Writes the Suggestion. Dismissing an accepted one is
 * an error. Free with castellum_string_free. */
int32_t castellum_dismiss_suggestion(uint64_t handle, const char *id, char **out_json);

/* castellum_count_suggestions: writes {"proposed": n}, the number awaiting a
 * decision (for a badge). Free with castellum_string_free. */
int32_t castellum_count_suggestions(uint64_t handle, char **out_json);

/* castellum_delete_work_item: permanent; the item's shared-link rows cascade. */
int32_t castellum_delete_work_item(uint64_t handle, const char *id);

/* ── Project activity + decision timeline (migration 0049) ──────────────── */
/* Append-only per-project log. Entries hold a short summary and reference
 * ids only (never message content). Work-item create / state change / delete,
 * a message linked to or unlinked from a project, and a message attached to a
 * work item are appended automatically by the core; every append bumps the
 * project's last_activity_at. TimelineEntry = {id, project_id, kind
 * (activity|decision|work_item_change), summary, ref_type (message|thread|
 * contact|work_item|null), ref_id|null, created_at (epoch seconds)}. */

/* castellum_add_timeline_entry: request_json = {project_id, kind
 * (activity|decision), summary, ref_kind?, ref_id?} (ref_kind and ref_id
 * together or neither; ref_kind in message|thread|contact|work_item). Writes
 * the stored TimelineEntry. work_item_change is written only by the core's
 * hooks. Free with castellum_string_free. */
int32_t castellum_add_timeline_entry(uint64_t handle, const char *request_json, char **out_json);

/* castellum_record_decision: request_json = {project_id, summary, ref_kind?,
 * ref_id?}. Writes the stored TimelineEntry (kind decision). */
int32_t castellum_record_decision(uint64_t handle, const char *request_json, char **out_json);

/* castellum_list_timeline: request_json = {project_id, kinds? (array),
 * limit? (default 50, max 500), offset? (default 0)}. Writes a JSON array of
 * TimelineEntry, newest first. Unknown keys are an error. */
int32_t castellum_list_timeline(uint64_t handle, const char *request_json, char **out_json);

/* castellum_list_work_item_provenance: a JSON array of {work_item_id,
 * source_kind (message|suggestion|manual), source_id, created_at}, oldest
 * first. Written when a suggestion is accepted and when an item is linked to a
 * message; work_items.notes is never rewritten. Free with castellum_string_free. */
int32_t castellum_list_work_item_provenance(uint64_t handle, const char *work_item_id, char **out_json);

/* castellum_record_work_item_provenance: request_json = {work_item_id,
 * source_kind: "manual", source_id?} (only "manual" is accepted; source_id is
 * ignored; message/suggestion rows are written only by the accept/link paths
 * and are rejected with an error). Idempotent; returns the stored row. Free with
 * castellum_string_free. */
int32_t castellum_record_work_item_provenance(uint64_t handle, const char *request_json, char **out_json);

/* castellum_work_items_from_source: request_json = {source_kind, source_id}.
 * Writes a JSON array of work-item ids. Free with castellum_string_free. */
int32_t castellum_work_items_from_source(uint64_t handle, const char *request_json, char **out_json);

/* ── Saved Active Work views (AW3) ──────────────────────────────────────── */
/* castellum_list_saved_views: the saved views in display order as a JSON
 * array of SavedView {id, name, filter, position, built_in, hidden,
 * created_at, updated_at}; `filter` is the work-item filter object. The four
 * built-ins (Today, Needs reply, Waiting on others, Someday) are seeded on
 * first use. include_hidden != 0 also lists hidden views. Free with
 * castellum_string_free. */
int32_t castellum_list_saved_views(uint64_t handle, int32_t include_hidden, char **out_json);

/* castellum_create_saved_view: request_json = {"name":"..","filter":{..}}.
 * The filter uses the castellum_list_work_items_filtered schema plus the
 * relative windows due:["today"|"this_week"|"overdue"] (OR-ed) and the
 * shorthands waiting_on_me / waiting_on_others (booleans). An invalid filter
 * (unknown key, state or window) or a duplicate/blank name is an error and
 * nothing is stored. Writes the stored SavedView. Free with
 * castellum_string_free. */
int32_t castellum_create_saved_view(uint64_t handle, const char *request_json, char **out_json);

/* castellum_update_saved_view: request_json = {"id":"..","name"?,"filter"?,
 * "hidden"?}. A built-in accepts only "hidden" (renaming or re-filtering it is
 * an error). Writes the stored SavedView. Free with castellum_string_free. */
int32_t castellum_update_saved_view(uint64_t handle, const char *request_json, char **out_json);

/* castellum_delete_saved_view: custom views only; a built-in is refused. */
int32_t castellum_delete_saved_view(uint64_t handle, const char *id);

/* castellum_reorder_saved_views: ids_json = JSON array of view ids. Listed ids
 * come first in that order, unknown ids are ignored, unlisted views keep their
 * relative order after them. Writes every view (hidden included) in the new
 * order. Free with castellum_string_free. */
int32_t castellum_reorder_saved_views(uint64_t handle, const char *ids_json, char **out_json);

/* castellum_saved_view_counts: the item count of every VISIBLE view in one
 * call (one SQL statement), in display order: [{"id":"..","count":N}]. Free
 * with castellum_string_free. */
int32_t castellum_saved_view_counts(uint64_t handle, char **out_json);

/* castellum_saved_view_items: the work items of one view as a JSON array of
 * WorkItem, evaluated by the same code as castellum_list_work_items_filtered
 * (due_at ascending, undated last). Free with castellum_string_free. */
int32_t castellum_saved_view_items(uint64_t handle, const char *id, char **out_json);

/* castellum_contact_health / castellum_thread_health: explainable
 * communication-health signals computed on demand from existing tables.
 * Output = {subject_kind, subject_id, as_of, signals:[{key, value|null, unit,
 * explanation, evidence_kind, evidence_ids:[..]}]} with keys reply_latency,
 * cadence_gap, unanswered_inbound, waiting_since_age; or the JSON string
 * "null" for an unknown contact/thread. Evidence is ids only (message ids;
 * work-item ids for waiting_since_age), never bodies. Free with
 * castellum_string_free. */
int32_t castellum_contact_health(uint64_t handle, int64_t contact_id, char **out_json);
int32_t castellum_thread_health(uint64_t handle, const char *thread_id, char **out_json);

/* castellum_list_unassigned_mail: one page of inbox threads with neither a
 * project link nor a work-item link, newest first. request_json = {"limit"?
 * (default 50, max 500),"offset"?} (NULL = defaults). Writes {"threads":
 * [{"thread_id","message_count","last_message_at"}],"total","limit","offset"}
 * — ids and counts only; fetch rows through the message-list API. Free with
 * castellum_string_free. */
int32_t castellum_list_unassigned_mail(uint64_t handle, const char *request_json, char **out_json);

/* ── IMAP IDLE supervision (S31) ────────────────────────────────────────── */
/* castellum_idle_start: start (or restart) an account's supervised IMAP IDLE
 * connection. config_json is the same shape castellum_sync_account takes.
 * castellum_idle_stop: stop it (blocks until the thread has exited); a no-op
 * if none was running. castellum_idle_nudge: wake it now (sleep/wake,
 * network change); a no-op if none was running. castellum_idle_running:
 * writes 1/0 to *out_running -- in-memory only, never persisted. True only
 * while a PROVEN, currently-idling connection is up: an account whose
 * server doesn't advertise IDLE, or whose IDLE keeps failing (backed off,
 * reconnecting), reports 0 so Swift's periodic poll stays its freshness
 * mechanism at its normal cadence rather than the slower idle-safety-net
 * one meant for an account IDLE is actually covering. */
int32_t castellum_idle_start(uint64_t handle, const char *config_json);
int32_t castellum_idle_stop(uint64_t handle, const char *account_id);
int32_t castellum_idle_nudge(uint64_t handle, const char *account_id);
int32_t castellum_idle_running(uint64_t handle, const char *account_id, int32_t *out_running);
/* castellum_idle_pass_epoch: writes to *out_epoch the number of IDLE-thread
 * resync passes finished in this process. A pass can rewrite message rows
 * (previews, bodies, sort keys) with no new arrival; the app polls this and
 * reloads the open list when it changes. In-memory only; starts at 0. */
int32_t castellum_idle_pass_epoch(uint64_t handle, uint64_t *out_epoch);

/* Closes the profile for handle. Also stops every account's supervised IDLE
 * thread (blocking until each has exited) before releasing the profile, so
 * a closed profile never leaves a background IMAP connection open.
 * Returns CASTELLUM_OK or CASTELLUM_INVALID_HANDLE. */
int32_t castellum_close_profile(uint64_t handle);

/* Trust surface (S48). */

/* Pure, stateless: analyse one link's (href, visible text) pair — real
 * destination host, text/href mismatch, punycode/IDN caution — as JSON in
 * *out_json. No profile handle needed; never fails on malformed input. Free
 * with castellum_string_free. */
int32_t castellum_inspect_link(const char *href, const char *visible_text, char **out_json);

/* Pure, stateless: what remote content a message's SANITIZED html tries to
 * load -- {"remote_count":n,"tracker_count":n,"pixel_count":n,
 * "services":[{"name","count"}],"hosts":[{"host","count"}]}. A tracker is a
 * 1x1/0x0 image or one served by a known mail-tracking service. Never fetches
 * anything; never fails on malformed html. Free with castellum_string_free. */
int32_t castellum_remote_content_report(const char *sanitized_html, char **out_json);

/* Remember (or update) whether from_address may load remote content
 * automatically for account_id. allowed != 0 means yes. */
int32_t castellum_set_sender_remote_content_allowed(uint64_t handle, const char *account_id,
                                                    const char *from_address, int32_t allowed);
/* Writes "null" (no remembered policy) or true/false to *out_json. */
int32_t castellum_sender_remote_content_allowed(uint64_t handle, const char *account_id,
                                                const char *from_address, char **out_json);
/* Every remembered sender policy across every account, as a JSON array. */
int32_t castellum_list_sender_remote_content_policies(uint64_t handle, char **out_json);
/* Forget a remembered sender policy; a no-op (not an error) if none existed. */
int32_t castellum_revoke_sender_remote_content(uint64_t handle, const char *account_id,
                                               const char *from_address);

/* CoreSpotlight indexing opt-in (S58, migration-free — one app_meta row).
 * Profile-wide, not per-account. Persists ONLY the flag: CoreSpotlight has
 * no C ABI, so the Swift side alone performs the resulting one-off backfill
 * (indexing message subject/sender metadata — never body/HTML/attachments)
 * or the domain-scoped CSSearchableIndex removal when turning it off.
 * castellum_spotlight_indexing_enabled is fail-closed: an unknown/never-set
 * profile reads 0 (off). */
int32_t castellum_set_spotlight_indexing_enabled(uint64_t handle, int32_t enabled);
int32_t castellum_spotlight_indexing_enabled(uint64_t handle, int32_t *out_enabled);

#ifdef __cplusplus
}
#endif

#endif /* CASTELLUM_H */
