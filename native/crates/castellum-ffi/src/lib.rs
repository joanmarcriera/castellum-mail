// Castellum - narrow C ABI for the Swift UI to call the Rust core.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Boundary rules (see project-docs/archive/electron/architecture-review/04-rust-migration.md):
//   * Only small, typed commands cross here — slice 1 exposes version/open/close.
//   * Handles are opaque `u64` keys into an internal registry (never raw Box
//     pointers), so a use-after-free is a clean InvalidHandle, not UB.
//   * Every entry point catches panics (a panic across the C boundary is UB) and
//     null-checks all pointers.
//   * Rust allocates the strings it returns; the caller MUST return them via
//     `castellum_string_free`.
//
// The C header is hand-written at ../include/castellum.h and MUST be kept in sync
// with the signatures below (5 functions; trivial to review).

pub mod logging;

use castellum_core::Profile;
use castellum_models::CastellumStatus;
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Handle -> open Profile. Process-global; guarded by a mutex. Profiles are held
/// behind `Arc` so a long-running command (sync) can clone the handle out and
/// release the registry lock before doing blocking work — otherwise `close`
/// (e.g. on app quit) would block for the whole sync.
static REGISTRY: OnceLock<Mutex<HashMap<u64, Arc<Profile>>>> = OnceLock::new();
/// Monotonic handle allocator. Starts at 1 so 0 is always an invalid sentinel.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<HashMap<u64, Arc<Profile>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the profile registry, recovering from mutex poisoning.
///
/// The registry is a plain handle map and is only ever held long enough to
/// insert, look up, or remove one entry, so an unwind cannot leave it
/// structurally torn. Treating poisoning as fatal (the old
/// `Err(_) => Internal` arm) made a single panic permanent: every later call,
/// on every profile, returned `Internal` until the app was restarted — the
/// bricked-profile symptom of the 2026-07-11 Gmail pilot bug. Recovering keeps
/// one bad sync from outliving itself. Mirrors `Database::lock_conn`.
fn registry_lock() -> std::sync::MutexGuard<'static, HashMap<u64, Arc<Profile>>> {
    registry().lock().unwrap_or_else(|e| {
        tracing::warn!("ffi profile registry mutex was poisoned by a prior panic; recovering");
        e.into_inner()
    })
}

thread_local! {
    /// The last error message on THIS thread, set whenever a command returns a
    /// non-OK status. Read via `castellum_last_error` right after a failed call
    /// (same thread) to get a human-readable reason behind the status code.
    static LAST_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn clear_last_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

fn set_last_error(msg: String) {
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(msg));
}

/// Install a process-wide panic hook (once) that records the panic's
/// `file:line` + message into the thread-local `last_error` BEFORE the stack
/// unwinds, so a panic caught by a `catch_unwind` wrapper surfaces as a
/// legible reason instead of an opaque "internal error". Rust's `unwrap`/
/// `expect`/indexing are `#[track_caller]`, so the location points at the real
/// failing line in our code. The previous hook is chained so the stderr
/// backtrace is preserved.
///
/// IMPORTANT: the payload is recorded VERBATIM, and not every panic is ours —
/// `imap`/`imap-proto` panic messages embed raw server bytes (see
/// `PanicGuardedProvider`), including for panics that guard catches and
/// sanitises. `last_error` is local-only (never persisted or exported), but it
/// IS read by the app and shown to the user, so every entry point must
/// `clear_last_error()` on entry and `set_last_error()` its own redacted text
/// on failure. `profile_json`/`profile_action` do both; anything hand-rolled
/// must too, or a caught-and-sanitised payload surfaces in the UI.
fn install_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "<unknown location>".to_string());
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_string());
            set_last_error(format!("internal panic at {location}: {message}"));
            previous(info);
        }));
    });
}

/// The last error message on the calling thread as a newly allocated C string
/// (free with `castellum_string_free`), or null if there was none.
#[no_mangle]
pub extern "C" fn castellum_last_error() -> *mut c_char {
    LAST_ERROR.with(|e| match e.borrow().as_deref() {
        Some(s) => CString::new(s)
            .map(|c| c.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    })
}

/// Return the native core version as a newly allocated, NUL-terminated UTF-8
/// string. The caller MUST free it with [`castellum_string_free`]. Returns null
/// only on an unexpected allocation failure.
#[no_mangle]
pub extern "C" fn castellum_version() -> *mut c_char {
    catch_unwind(|| match CString::new(castellum_core::version()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    })
    .unwrap_or(std::ptr::null_mut())
}

/// Render Markdown to sanitized HTML for the compose window's live preview
/// (S46). Stateless — no profile handle, mirrors `castellum_version`'s
/// shape. Returns a newly allocated C string (free with
/// `castellum_string_free`); a null/invalid `markdown` pointer renders as
/// the empty string rather than failing (there is no error channel here —
/// the caller always gets back A string, matching `castellum_version`'s
/// "always succeeds" contract).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_render_markdown_preview(markdown: *const c_char) -> *mut c_char {
    catch_unwind(|| {
        let text = unsafe { cstr(markdown) }.unwrap_or_default();
        let html = castellum_core::compose::render_markdown_preview(&text);
        CString::new(html).unwrap_or_default().into_raw()
    })
    .unwrap_or_else(|_| CString::new(String::new()).unwrap().into_raw())
}

/// Themed variant of `castellum_render_markdown_preview` (MM-5): `theme` is one
/// of "plain", "clean", "serif", "compact" (null or unknown = plain). Same
/// ownership contract: the returned string is freed with
/// `castellum_string_free`; always returns a string, never null.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_render_markdown_themed(
    markdown: *const c_char,
    theme: *const c_char,
) -> *mut c_char {
    catch_unwind(|| {
        let text = unsafe { cstr(markdown) }.unwrap_or_default();
        let theme = unsafe { cstr(theme) }.unwrap_or_default();
        let html = castellum_core::compose::render_markdown_themed(&text, &theme);
        CString::new(html).unwrap_or_default().into_raw()
    })
    .unwrap_or_else(|_| CString::new(String::new()).unwrap().into_raw())
}

/// Free a string previously returned by this library. Safe to call with null.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // Reclaim ownership and drop. Wrapped so a panic can't cross the boundary.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(CString::from_raw(s));
    }));
}

/// Open (creating if needed) the profile at `path_utf8` and write its handle to
/// `out_handle`. Returns a [`CastellumStatus`] code. On success `*out_handle` is
/// non-zero.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_open_profile(path_utf8: *const c_char, out_handle: *mut u64) -> i32 {
    install_panic_hook();
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if path_utf8.is_null() || out_handle.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        let path_str = match unsafe { CStr::from_ptr(path_utf8) }.to_str() {
            Ok(s) => s,
            Err(_) => return CastellumStatus::InvalidArgument,
        };
        // Once-per-process, keyed off the first profile opened — see
        // logging.rs's doc comment for why that's the right scope (mirrors
        // install_panic_hook above).
        logging::init_logging(Path::new(path_str));
        match castellum_core::open_profile(Path::new(path_str)) {
            Ok(profile) => {
                let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
                let mut map = registry_lock();
                map.insert(handle, Arc::new(profile));
                unsafe { *out_handle = handle };
                CastellumStatus::Ok
            }
            Err(e) => {
                // Unlike most FFI entry points here, `open_profile` can fail
                // with a message the UI must show verbatim (the
                // SchemaTooNew/MigrationFailed launch refusals) — `set_last_error`
                // is what makes it reachable via `castellum_last_error`
                // (the status code alone can't distinguish these; see
                // `CoreError::status`'s doc comment).
                set_last_error(e.to_string());
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Cheaply report how many migrations are pending for the profile at
/// `path_utf8`, WITHOUT opening it for real or running any of them — see
/// `castellum_core::pending_migrations`'s doc comment. Writes the count to
/// `out_pending_count`; when non-null, `out_has_heavy` gets whether any
/// pending migration is heavy (0/1), and `out_applied_version` gets the
/// applied schema watermark the probe observed (0 for a profile that
/// doesn't exist yet or was never migrated). The Swift launch preflight
/// needs BOTH `out_applied_version` and `out_pending_count`
/// to decide whether to show the "Upgrading your mail database…" window:
/// `applied_version == 0` is a brand-new profile — `pending_count` is also
/// nonzero there (every migration this build knows), but there is nothing
/// to protect and creating+migrating it from empty is fast enough to need
/// no window. Only `applied_version > 0 && pending_count > 0` is an
/// EXISTING profile with real migrating work ahead of it. No handle is
/// needed or created; this never registers anything in the profile
/// registry.
///
/// Same convention as `castellum_open_profile` for the launch-blocking
/// refusal: a profile whose applied schema is newer than this build
/// supports (#169's `SchemaTooNew`) returns `CastellumStatus::Io` with the
/// classified message set via `castellum_last_error` — the Swift side must
/// check that BEFORE trusting the out-params, exactly as it already does
/// for `castellum_open_profile`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_pending_migration_count(
    path_utf8: *const c_char,
    out_pending_count: *mut i64,
    out_has_heavy: *mut u8,
    out_applied_version: *mut i64,
) -> i32 {
    install_panic_hook();
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if path_utf8.is_null() || out_pending_count.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        let path_str = match unsafe { CStr::from_ptr(path_utf8) }.to_str() {
            Ok(s) => s,
            Err(_) => return CastellumStatus::InvalidArgument,
        };
        match castellum_core::pending_migrations(Path::new(path_str)) {
            Ok(info) => {
                unsafe {
                    *out_pending_count = info.pending_count;
                    if !out_has_heavy.is_null() {
                        *out_has_heavy = u8::from(info.has_heavy);
                    }
                    if !out_applied_version.is_null() {
                        *out_applied_version = info.applied_version;
                    }
                }
                CastellumStatus::Ok
            }
            Err(e) => {
                set_last_error(e.to_string());
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Write the applied database schema version for `handle` to `out_version`.
/// Returns [`CastellumStatus::Ok`], or `InvalidHandle`/`InvalidArgument`/`Io`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_profile_schema_version(handle: u64, out_version: *mut i64) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_version.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        match registry_lock().get(&handle) {
            Some(profile) => match profile.schema_version() {
                Ok(v) => {
                    unsafe { *out_version = v };
                    CastellumStatus::Ok
                }
                Err(e) => e.status(),
            },
            None => CastellumStatus::InvalidHandle,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Return the directory this profile's log files live in (a newly allocated
/// UTF-8 string, free with [`castellum_string_free`]), or null for an
/// invalid handle. Additive diagnostics helper (observability slice): lets a
/// future Swift diagnostics/support feature locate the Rust log file without
/// hardcoding `<profile>/logs` on its side. The directory may not contain any
/// files yet (nothing has logged at INFO+ since the profile opened) — that is
/// not an error, callers should treat a missing/empty directory as "nothing
/// to show" rather than a failure.
#[no_mangle]
pub extern "C" fn castellum_log_directory(handle: u64) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        if handle == 0 {
            return std::ptr::null_mut();
        }
        match registry_lock().get(&handle) {
            Some(profile) => {
                let dir = logging::log_directory(profile.path());
                CString::new(dir.to_string_lossy().into_owned())
                    .map(|c| c.into_raw())
                    .unwrap_or(std::ptr::null_mut())
            }
            None => std::ptr::null_mut(),
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Sync an IMAP account into the profile. `config_json` is a UTF-8 JSON object
/// (account_id, email, host, port, username, password, use_tls, allow_invalid_certs,
/// imap_security? ("tls" | "starttls" | "none", which wins over use_tls; the tracker
/// 3375), mailboxes?). The same optional key applies to every function taking
/// the stored-account shape. On success writes a newly allocated JSON report to `*out_json`
/// (free with [`castellum_string_free`]). Read-only; never mutates remote state.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_sync_account(
    handle: u64,
    config_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    // Routed through `profile_json` like every other command, deliberately.
    // It clears `last_error` on entry and records the CORE's error text on
    // failure. A hand-rolled arm that only returned `e.status()` left whatever
    // the panic hook had written in place — including the raw payload of a
    // panic `PanicGuardedProvider` had already caught and sanitised, which
    // embeds raw server bytes — for Swift's `lastReason()` to show in the sync
    // alert and for a later, unrelated failure to re-show.
    // `profile_json` also clones the Arc and drops the registry lock before
    // running, so the blocking network sync still holds no lock.
    profile_json(handle, out_json, |p| p.sync_imap_json(&config_json))
}

/// Start (or restart) an account's supervised IMAP IDLE connection.
/// `config_json` is the same shape `castellum_sync_account` takes. Returns
/// once the background thread has been spawned — it does not wait for the
/// connection to actually succeed (a connect failure is retried with backoff
/// inside the thread and surfaces later via the account's ordinary sync
/// diagnostics, the same as any other sync failure).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_idle_start(handle: u64, config_json: *const c_char) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        let profile = {
            let map = registry_lock();
            match map.get(&handle) {
                Some(p) => Arc::clone(p),
                None => return CastellumStatus::InvalidHandle,
            }
        };
        match profile.start_idle(&config_json) {
            Ok(()) => CastellumStatus::Ok,
            Err(e) => {
                set_last_error(e.to_string());
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Stop an account's IDLE thread if one is running; blocks until it has
/// fully exited. A no-op (still `Ok`) if none was running.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_idle_stop(handle: u64, account_id: *const c_char) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.stop_idle(&account_id);
        Ok(())
    })
}

/// Wake an account's IDLE thread now (sleep/wake, network change). A no-op
/// if that account has no running thread.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_idle_nudge(handle: u64, account_id: *const c_char) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.nudge_idle(&account_id);
        Ok(())
    })
}

/// Whether an account currently has a PROVEN, actively-idling IDLE
/// connection, written to `*out_running` (1/0). In-memory only — never
/// persisted. False while connecting, backed off after a failure, or once
/// the server has been confirmed not to advertise IDLE — never merely
/// "a background thread object still exists".
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_idle_running(
    handle: u64,
    account_id: *const c_char,
    out_running: *mut i32,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_running.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        match registry_lock().get(&handle) {
            Some(profile) => {
                unsafe { *out_running = profile.is_idle_running(&account_id) as i32 };
                CastellumStatus::Ok
            }
            None => CastellumStatus::InvalidHandle,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Writes the count of finished IDLE-thread resync passes (in this process) to
/// `*out_epoch`. A pass can rewrite message rows with no new arrival, so the
/// app polls this and reloads the open list when it changes (#3040).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_idle_pass_epoch(handle: u64, out_epoch: *mut u64) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_epoch.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        match registry_lock().get(&handle) {
            Some(profile) => {
                unsafe { *out_epoch = profile.idle_pass_epoch() };
                CastellumStatus::Ok
            }
            None => CastellumStatus::InvalidHandle,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Best-effort snapshot of `account_id`'s CURRENT (or most recently
/// finished) sync pass — mailbox/message counts, for a non-blocking progress
/// UI to poll alongside a long-running `castellum_sync_account`
/// call on another thread. Writes a JSON `SyncProgress` object, or the JSON
/// `null` literal when no sync has run for this account yet this process —
/// never an error for that case. Local-only; no network I/O.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_sync_progress(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.sync_progress_json(&account_id))
}

/// Account-setup preflight (S62): verify IMAP (connect, LOGIN, LIST) and,
/// when `config_json` carries an `smtp` object, SMTP (connect, AUTH) WITHOUT
/// persisting anything. Writes a JSON `ConnectionTestReport` — a connection
/// failure is reported inside it (`imap.ok == false` + a redacted
/// `error_kind`), not as a non-OK status; only a malformed request fails the
/// call. Blocking network I/O: call off the main thread. Routed through
/// `profile_json` for the usual `last_error` + panic handling.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_test_connection(
    handle: u64,
    config_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.test_connection_json(&config_json))
}

/// Persist an account's identity (id/email, parsed from the same
/// `config_json` shape `castellum_sync_account` takes) WITHOUT syncing — the
/// "Save anyway" action when the connection check failed. Makes the account
/// visible to `castellum_list_accounts`; does not touch sync/diagnostic
/// state, which a prior failed `castellum_sync_account` call already
/// recorded. Never performs network I/O.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_save_account_without_sync(
    handle: u64,
    config_json: *const c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.save_account_without_sync_json(&config_json))
}

/// Last-run sync KPI snapshot for this profile as JSON in `*out_json` (free
/// with [`castellum_string_free`]): per-phase timings, message count,
/// timestamp and account id from the most recent `castellum_sync_account`
/// call, written by `Profile::sync_imap_json`
/// (`castellum_core::sync_kpi::write_last_sync_kpi`). A future Diagnostics UI
/// consumes this to show "what did the last sync cost" without a sync in
/// progress. `*out_json` is the JSON string `"null"` (not a failure status)
/// when no sync has completed in this profile yet. Local-only, no network
/// I/O; a plain file read.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_last_sync_kpi(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.last_sync_kpi_json())
}

/// Hydrate one descriptor-only message body from IMAP and return the refreshed
/// MessageDetail JSON. `config_json` carries account credentials; unlike
/// `castellum_get_message`, this may perform blocking network I/O.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_hydrate_message_body(
    handle: u64,
    config_json: *const c_char,
    message_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(config_json), Some(message_id)) =
        (unsafe { cstr(config_json) }, unsafe { cstr(message_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.hydrate_message_body_json(&config_json, &message_id)
    })
}

/// Run one bounded background body-hydration pass over the account's durable
/// missing-body queue and write a JSON `HydrationReport` to `*out_json`.
/// `config_json` carries account credentials; an empty queue returns without
/// any network I/O. Callers re-invoke while the report shows progress and
/// remaining backlog.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_hydrate_pending_bodies(
    handle: u64,
    config_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.hydrate_pending_bodies_json(&config_json)
    })
}

/// Record the user's Accept/Decline/Tentative response (S55) to
/// `message_id`'s calendar invite and queue an iTIP REPLY to its organizer
/// through the normal outbox (the caller still has to drain it, same as any
/// other queued send — see `castellum_send_draft`/`castellum_send_pending`).
/// `response` must be `"accepted"`, `"declined"` or `"tentative"`. Writes
/// `{"draft_id": i64}` to `*out_json` on success. Local-only — never opens a
/// network connection itself. When the invite's ORGANIZER matches neither the
/// message's From nor Reply-To the call fails with a "recipient mismatch:
/// <address>" message unless `confirmed_recipient` is that exact address.
///
/// `confirmed_recipient` is NULL when the user has confirmed nothing, else the
/// address the confirmation prompt named (UTF-8, NUL-terminated). It is bound
/// to the send (#3472): a non-NULL value that is not exactly the address the
/// reply would go to fails with the same "recipient mismatch: <address>"
/// message naming the current address. A non-NULL pointer that is not valid
/// UTF-8 is `InvalidArgument`, never treated as "nothing confirmed".
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_respond_to_calendar_invite(
    handle: u64,
    message_id: *const c_char,
    response: *const c_char,
    confirmed_recipient: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(message_id), Some(response)) =
        (unsafe { cstr(message_id) }, unsafe { cstr(response) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    let confirmed = if confirmed_recipient.is_null() {
        None
    } else {
        match unsafe { cstr(confirmed_recipient) } {
            Some(address) => Some(address),
            None => return CastellumStatus::InvalidArgument.code(),
        }
    };
    profile_json(handle, out_json, |p| {
        p.respond_to_calendar_invite_json(&message_id, &response, confirmed.as_deref())
    })
}

/// Local-only: is this attachment already cached, and where. Never opens a
/// network connection.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_attachment_cache_status(
    handle: u64,
    message_id: *const c_char,
    ordinal: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.attachment_cache_status_json(&message_id, ordinal)
    })
}

/// Download one incoming attachment part over IMAP and cache it locally.
/// `config_json` carries account credentials (same shape as
/// `castellum_hydrate_message_body`). Blocking network I/O unless already
/// cached — check `castellum_attachment_cache_status` first to avoid a
/// needless connection.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_download_attachment(
    handle: u64,
    config_json: *const c_char,
    message_id: *const c_char,
    ordinal: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(config_json), Some(message_id)) =
        (unsafe { cstr(config_json) }, unsafe { cstr(message_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.download_attachment_json(&config_json, &message_id, ordinal)
    })
}

/// List a draft's attachment metadata (no bytes) as JSON `[DraftAttachmentMeta]`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_draft_attachments(
    handle: u64,
    draft_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.list_draft_attachments_json(draft_id)
    })
}

/// Add one attachment to a draft. `attachment_json` is a JSON
/// `{filename, content_type, is_inline?, content_id?, data_base64}` object.
/// Writes the draft's refreshed attachment list JSON to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_add_draft_attachment(
    handle: u64,
    draft_id: i64,
    attachment_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(attachment_json) = (unsafe { cstr(attachment_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.add_draft_attachment_json(draft_id, &attachment_json)
    })
}

/// Embed one image in a draft as an inline part (MM-5b). `image_json` is
/// `{filename, alt?, data_base64}`; the core decides the type from the bytes
/// (PNG, JPEG, GIF, WebP) and generates the Content-ID. Writes
/// `{"attachment", "markdown", "attachments"}` to `*out_json`. A non-image or
/// an image over 10 MB returns InvalidArgument and writes nothing.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_embed_draft_image(
    handle: u64,
    draft_id: i64,
    image_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(image_json) = (unsafe { cstr(image_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.embed_draft_image_json(draft_id, &image_json)
    })
}

/// Render Markdown for a draft (MM-5b): like `castellum_render_markdown_themed`,
/// but `![x](cid:ID)` for an inline attachment of `draft_id` becomes an
/// `<img>`. Writes the HTML to `*out_html`; free with `castellum_string_free`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_render_markdown_for_draft(
    handle: u64,
    draft_id: i64,
    markdown: *const c_char,
    theme: *const c_char,
    out_html: *mut *mut c_char,
) -> i32 {
    let (Some(markdown), Some(theme)) = (unsafe { cstr(markdown) }, unsafe { cstr(theme) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_html, |p| {
        p.render_markdown_for_draft(draft_id, &markdown, &theme)
    })
}

/// Local-only (MM-5b): `{"content_type", "data_base64"}` of the inline
/// attachment of `draft_id` with this Content-ID, for the compose preview.
/// Anything that is not PNG/JPEG/GIF/WebP returns InvalidArgument.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_draft_inline_image(
    handle: u64,
    draft_id: i64,
    content_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(content_id) = (unsafe { cstr(content_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.draft_inline_image_json(draft_id, &content_id)
    })
}

/// Make an existing draft attachment inline (`inline_flag != 0`) or a regular
/// attachment (`0`) (MM-5b). Writes `{"attachments": [...], "markdown": string|null}`
/// to `*out_json`; `markdown` is the reference to insert when it became inline.
/// A non-image, an image over 10 MB, or an unknown attachment returns
/// InvalidArgument; a draft that has left `draft` status is refused.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_draft_attachment_inline(
    handle: u64,
    draft_id: i64,
    attachment_id: i64,
    inline_flag: i32,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.set_draft_attachment_inline_json(draft_id, attachment_id, inline_flag != 0)
    })
}

/// Remove one attachment from a draft. Writes the draft's refreshed
/// attachment list JSON to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_remove_draft_attachment(
    handle: u64,
    draft_id: i64,
    attachment_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.remove_draft_attachment_json(draft_id, attachment_id)
    })
}

/// Copy every attachment of `message_id` into `draft_id` (Forward),
/// downloading any that are not already locally cached. `config_json` carries
/// the SOURCE message's account credentials (same shape as
/// `castellum_download_attachment`). Blocking network I/O. Writes the draft's
/// refreshed attachment list JSON to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_forward_attachments(
    handle: u64,
    config_json: *const c_char,
    message_id: *const c_char,
    draft_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(config_json), Some(message_id)) =
        (unsafe { cstr(config_json) }, unsafe { cstr(message_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.forward_attachments_json(&config_json, &message_id, draft_id)
    })
}

/// Local-only: is this message's raw RFC822 source already cached, and
/// where. Never opens a network connection.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_raw_source_cache_status(
    handle: u64,
    message_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.raw_source_cache_status_json(&message_id)
    })
}

/// Fetch one message's full raw RFC822 source over IMAP and cache it.
/// `config_json` carries account credentials (same shape as
/// `castellum_hydrate_message_body`). Blocking network I/O unless already
/// cached — check `castellum_raw_source_cache_status` first to avoid a
/// needless connection. Feeds both "Message Source" and `.eml` export.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_fetch_raw_source(
    handle: u64,
    config_json: *const c_char,
    message_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(config_json), Some(message_id)) =
        (unsafe { cstr(config_json) }, unsafe { cstr(message_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.fetch_raw_source_json(&config_json, &message_id)
    })
}

/// Read, parse and queue a local `.eml` file at `file_path` for import into
/// `mailbox_id` on `account_id`. No network I/O — the import replays via
/// the durable operation queue like every other queued mutation.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_import_eml(
    handle: u64,
    account_id: *const c_char,
    mailbox_id: *const c_char,
    file_path: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(account_id), Some(mailbox_id), Some(file_path)) = (
        unsafe { cstr(account_id) },
        unsafe { cstr(mailbox_id) },
        unsafe { cstr(file_path) },
    ) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.import_eml_json(&account_id, &mailbox_id, &file_path)
    })
}

/// Generate the allowlisted, redacted support bundle as JSON in `*out_json`.
/// Strictly read-only: never writes to the database, never opens a network
/// connection, and never includes credentials, message content, folder
/// names, or raw error text.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_support_bundle(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.support_bundle_json())
}

/// Run the read-only SQLite integrity check (`PRAGMA quick_check`) and write
/// `{"ok": bool, "issue_count": n, "first_issue": string|null}` to
/// `*out_json`. Never modifies the profile.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_integrity_check(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.integrity_check_json())
}

/// Best-effort redaction of free text (crash reports, unified-log excerpts)
/// for diagnostics export — NOT the allowlist path `castellum_support_bundle`
/// uses. Writes a newly allocated string to `*out_text` (free with
/// `castellum_string_free`). No profile handle is required: this is a pure
/// text transform, so the Swift diagnostics-gathering code can call it
/// without an open profile.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_redact_diagnostic_text(
    input_utf8: *const c_char,
    out_text: *mut *mut c_char,
) -> i32 {
    install_panic_hook();
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_text.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        let Some(input) = (unsafe { cstr(input_utf8) }) else {
            return CastellumStatus::InvalidArgument;
        };
        let redacted = castellum_core::redact::redact_diagnostic_text(&input);
        match CString::new(redacted) {
            Ok(s) => {
                unsafe { *out_text = s.into_raw() };
                CastellumStatus::Ok
            }
            Err(_) => CastellumStatus::Internal,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Account-setup auto-detect. Looks up IMAP/SMTP
/// settings for a mail DOMAIN (DNS SRV, MX hint, the domain's own autoconfig,
/// Mozilla ISPDB; see `castellum_core::autoconfig`). `request_json` is
/// `{"domain": "...", "sources"?: [...], "timeout_ms"?: n}`; it refuses any
/// other field, so the address's local part cannot be passed in. No profile
/// handle: stateless, persists nothing, never contacts the servers it
/// returns. A failed or empty lookup is reported INSIDE the JSON report
/// (`not_found` / `offline`); only a malformed request returns a non-OK status.
/// Blocking network I/O bounded by the request's deadline (default 5 s) --
/// the caller must run it off the main thread.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_autoconfig_lookup(
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    install_panic_hook();
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_json.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        let Some(request) = (unsafe { cstr(request_json) }) else {
            return CastellumStatus::InvalidArgument;
        };
        match castellum_core::autoconfig::lookup_json(&request) {
            Ok(json) => match CString::new(json) {
                Ok(s) => {
                    unsafe { *out_json = s.into_raw() };
                    CastellumStatus::Ok
                }
                Err(_) => CastellumStatus::Internal,
            },
            Err(e) => {
                // The request text is never echoed (it holds only a domain, but
                // a caller bug could put anything in it).
                set_last_error(format!("autoconfig lookup rejected: {}", e.status().code()));
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Copy a C string into an owned `String`; `None` on null or invalid UTF-8.
/// Owning (rather than borrowing) keeps the FFI arguments valid for the whole
/// call with no lifetime tricks — args here are short IDs/queries, so it's cheap.
///
/// # Safety
/// `p` must be null or a valid NUL-terminated C string.
unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().map(str::to_owned)
}

/// Like [`cstr`] but distinguishes NULL from invalid UTF-8: `Some(None)` for a
/// NULL pointer, `Some(Some(s))` for a valid string, `None` for a non-NULL
/// pointer that is not UTF-8.
unsafe fn opt_cstr(p: *const c_char) -> Option<Option<String>> {
    if p.is_null() {
        return Some(None);
    }
    CStr::from_ptr(p).to_str().ok().map(|s| Some(s.to_owned()))
}

/// Run a JSON-producing read query against the profile for `handle`, writing the
/// result to `*out_json` (caller frees via [`castellum_string_free`]). Clones the
/// Arc out before running so the registry lock isn't held during the query.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
fn profile_json<F>(handle: u64, out_json: *mut *mut c_char, f: F) -> i32
where
    F: FnOnce(&Profile) -> Result<String, castellum_core::CoreError>,
{
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_json.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        let profile = {
            let map = registry_lock();
            match map.get(&handle) {
                Some(p) => Arc::clone(p),
                None => return CastellumStatus::InvalidHandle,
            }
        };
        match f(&profile) {
            Ok(json) => match CString::new(json) {
                Ok(s) => {
                    unsafe { *out_json = s.into_raw() };
                    CastellumStatus::Ok
                }
                Err(_) => CastellumStatus::Internal,
            },
            Err(e) => {
                set_last_error(e.to_string());
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// List accounts as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_accounts(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.list_accounts_json())
}

/// Permanently delete `account_id` and every local mail record it owns
/// (mailboxes, messages, sync cursors, queued operations, drafts). Does NOT
/// touch the Keychain — the Swift `AccountStore` removes stored credentials
/// on its own side of the C ABI. Returns a CastellumStatus code; errors when
/// the account does not exist.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_remove_account(handle: u64, account_id: *const c_char) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_account(&account_id))
}

/// What an unforced `castellum_reset_account` would refuse on, as JSON
/// ({"unsent_outbox_drafts": n, "pending_operations": n}) in `*out_json` —
/// the pre-check the confirmation dialog reads.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_reset_account_blockers(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.reset_account_blockers_json(&account_id)
    })
}

/// Wipes account_id's local mail state (mailboxes, messages, bodies, FTS,
/// sync cursors, AI artefacts) — keeps the account row, Keychain credentials
/// (never touched here), write policy, signatures and any rules. Refuses
/// (status Io; read the reason via castellum_last_error, prefixed "reset
/// blocked:") when the account has an unsent outbox draft or a non-terminal
/// queued operation, UNLESS force != 0. Errors when the account is unknown.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_reset_account(
    handle: u64,
    account_id: *const c_char,
    force: i32,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.reset_account(&account_id, force != 0))
}

/// List mailboxes for `account_id` as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_mailboxes(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.list_mailboxes_json(&account_id))
}

/// Per-mailbox total/unread counts for `account_id` as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_mailbox_counts(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.mailbox_counts_json(&account_id))
}

/// Per-mailbox body-hydration progress for `account_id` as a JSON array of
/// `{"mailbox_id","total","hydrated"}` in `*out_json` (#3631 G5). Hydration is
/// budgeted, so `hydrated < total` is normal on a fresh large account.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_body_hydration_progress(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.body_hydration_progress_json(&account_id)
    })
}

/// Body-hydration coverage of a search scope as `{"total","missing"}` in
/// `*out_json` (#3633 G13). `account_id` and `mailbox_id` are each optional
/// (NULL = not narrowed; both NULL = every account); a non-NULL `mailbox_id`
/// is a mailbox id, normally with its `account_id`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_body_hydration_scope(
    handle: u64,
    account_id: *const c_char,
    mailbox_id: *const c_char,
    query: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    // NULL = "not narrowed", but a non-NULL pointer that is not UTF-8 is a
    // caller bug: refuse it rather than silently widening the count.
    let (Some(account_id), Some(mailbox_id), Some(query)) = (
        unsafe { opt_cstr(account_id) },
        unsafe { opt_cstr(mailbox_id) },
        unsafe { opt_cstr(query) },
    ) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.body_hydration_scope_json(
            account_id.as_deref(),
            mailbox_id.as_deref(),
            query.as_deref(),
        )
    })
}

/// What an unforced `castellum_apply_mailbox_selection` would refuse on, as a
/// JSON array of `{"path": ..., "pending_operations": n}` in `*out_json` —
/// the pre-check the confirmation dialog reads ( follow-up
/// review). See `Database::apply_mailbox_selection_blockers`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_apply_mailbox_selection_blockers(
    handle: u64,
    account_id: *const c_char,
    keep_paths_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let Some(keep_paths_json) = (unsafe { cstr(keep_paths_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.apply_mailbox_selection_blockers_json(&account_id, &keep_paths_json)
    })
}

/// Apply an edited folder selection to an ALREADY-ADDED account (the tracker
/// #2617, Settings > Accounts > Folders…). `keep_paths_json` is a JSON array
/// of the paths still selected (must not be empty); every other locally known
/// mailbox for `account_id` is removed right away — its messages cascade away
/// — UNLESS a non-terminal operation still touches it, in which case the
/// WHOLE call is refused (status Io; reason prefixed "mailbox removal
/// blocked:") and nothing is changed, matching `castellum_reset_account`'s
/// refusal idiom. Read `castellum_apply_mailbox_selection_blockers` first to
/// word that refusal; pass `force != 0` only on explicit user confirmation,
/// which cancels those specific operations with a truthful reason instead of
/// the UIDVALIDITY-purge one (see `Database::apply_mailbox_selection`).
/// Never touches the server or the Keychain. Writes `{"removed": n}` to
/// `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_apply_mailbox_selection(
    handle: u64,
    account_id: *const c_char,
    keep_paths_json: *const c_char,
    force: i32,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let Some(keep_paths_json) = (unsafe { cstr(keep_paths_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.apply_mailbox_selection_json(&account_id, &keep_paths_json, force != 0)
    })
}

/// Like `profile_json` but for mutations with no JSON output.
fn profile_action<F>(handle: u64, f: F) -> i32
where
    F: FnOnce(&Profile) -> Result<(), castellum_core::CoreError>,
{
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        let profile = {
            let map = registry_lock();
            match map.get(&handle) {
                Some(p) => Arc::clone(p),
                None => return CastellumStatus::InvalidHandle,
            }
        };
        match f(&profile) {
            Ok(()) => CastellumStatus::Ok,
            Err(e) => {
                set_last_error(e.to_string());
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Mark a message read (is_read != 0) or unread: applies locally + enqueues a
/// replayable operation. Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_mark_read(handle: u64, message_id: *const c_char, is_read: i32) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.mark_read(&message_id, is_read != 0))
}

/// Mark a message read as a SIDE EFFECT of opening it (auto-mark-read).
/// Unlike [`castellum_mark_read`] this is never refused in evaluation mode: it
/// applies the local flag and, only when writes are enabled, enqueues the
/// replayable op. Reading mail must not require leaving read-only. Callers
/// must use `castellum_mark_read` for an explicit Toggle Read/Unread.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_mark_read_auto(
    handle: u64,
    message_id: *const c_char,
    is_read: i32,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.mark_read_auto(&message_id, is_read != 0))
}

/// Mark every unread message in a mailbox read: applies locally + enqueues one
/// bulk replayable operation. Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_mark_all_read(
    handle: u64,
    account_id: *const c_char,
    mailbox_id: *const c_char,
) -> i32 {
    let (Some(account_id), Some(mailbox_id)) =
        (unsafe { cstr(account_id) }, unsafe { cstr(mailbox_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.mark_all_read(&account_id, &mailbox_id).map(|_| ())
    })
}

/// Set (`flagged != 0`) or clear a message's `\Flagged` state: local + enqueue.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_flagged(
    handle: u64,
    message_id: *const c_char,
    flagged: i32,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.set_flagged(&message_id, flagged != 0))
}

/// Snooze `message_id` until `until_epoch` (Unix epoch seconds).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_snooze_message(
    handle: u64,
    message_id: *const c_char,
    until_epoch: i64,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.snooze_message(&message_id, until_epoch))
}

/// Un-snooze `message_id` now. Writes `{"unsnoozed": bool}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_unsnooze_message(
    handle: u64,
    message_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.unsnooze_message_json(&message_id))
}

/// List every currently-snoozed message, soonest due first, paginated — the
/// Snoozed sidebar group.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_snoozed_messages(
    handle: u64,
    limit: i64,
    offset: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.list_snoozed_messages_json(limit, offset)
    })
}

/// Pop every snooze due at or before `now_epoch`. Writes a JSON array of
/// `{"message_id":..., "account_id":...}` to `*out_json`. Call on launch and
/// on a periodic sweep (SQLite is the only source of truth — safe to call
/// repeatedly and after any crash).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_unsnooze_due(
    handle: u64,
    now_epoch: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.unsnooze_due_json(now_epoch))
}

/// List every tag definition, in display order. Writes a JSON array to
/// `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_tags(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.list_tags_json())
}

/// Create (empty `keyword`) or update a tag definition from a JSON
/// `TagDefinition` in `tag_json`. Writes the stored row to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_upsert_tag(
    handle: u64,
    tag_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(tag_json) = (unsafe { cstr(tag_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.upsert_tag_json(&tag_json))
}

/// Delete a tag definition (keyed by `keyword_key`). The keyword itself
/// stays on the server and on any message that already carries it.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_tag(handle: u64, keyword: *const c_char) -> i32 {
    let Some(keyword) = (unsafe { cstr(keyword) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_tag(&keyword))
}

/// Add (`add != 0`) or remove one IMAP keyword on a message. Writes
/// `{"synced": bool, "reason": null|"evaluation_mode"|"not_permanent"}` to
/// `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_message_tag(
    handle: u64,
    message_id: *const c_char,
    keyword: *const c_char,
    add: i32,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(message_id), Some(keyword)) = (unsafe { cstr(message_id) }, unsafe { cstr(keyword) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.set_message_tag_json(&message_id, &keyword, add != 0)
    })
}

/// Mute (`muted != 0`) or unmute `message_id`'s whole conversation. Local
/// only. Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_thread_muted(
    handle: u64,
    message_id: *const c_char,
    muted: i32,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.set_thread_muted(&message_id, muted != 0))
}

/// Every distinct visible keyword in use with a message count. Writes a
/// JSON array to `*out_json` — Settings' "In use on your servers" adopt
/// list.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_keywords_in_use(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.list_keywords_in_use_json())
}

/// `[{keyword_key, total, unread, local_only}]` for every defined tag. Writes
/// a JSON array to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_tag_counts(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.tag_counts_json())
}

/// MM-8: the MessageHeader array for one defined tag across every account
/// (Trash and Junk excluded, the same rule as `castellum_tag_counts`),
/// newest first, keyset-paged: `after_id` NULL = first page, else the id and
/// `after_sent_at` (`sent_at`, or 0 when null) of the last row of the previous
/// page. An unknown keyword writes `[]`.
/// Local only, no network I/O.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_messages_for_tag(
    handle: u64,
    keyword: *const c_char,
    limit: i64,
    after_sent_at: i64,
    after_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(keyword) = (unsafe { cstr(keyword) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let after_id = if after_id.is_null() {
        None
    } else {
        match unsafe { cstr(after_id) } {
            Some(id) => Some(id),
            None => return CastellumStatus::InvalidArgument.code(),
        }
    };
    profile_json(handle, out_json, |p| {
        p.messages_for_tag_json(
            &keyword,
            limit,
            after_id.as_deref().map(|id| (after_sent_at, id)),
        )
    })
}

/// Add (`vip != 0`) or remove a VIP sender (S59). `display_name` may be
/// NULL. Writes `{"changed": bool}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_vip(
    handle: u64,
    address: *const c_char,
    display_name: *const c_char,
    vip: i32,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(address) = (unsafe { cstr(address) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let display_name = unsafe { cstr(display_name) };
    profile_json(handle, out_json, |p| {
        p.set_vip_json(&address, display_name.as_deref(), vip != 0)
    })
}

/// Every VIP sender as a JSON array of `{address, display_name, created_at}`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_vips(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.list_vips_json())
}

/// Pop up to `limit` undelivered new-mail notices as a JSON array of
/// `PendingNotification` and mark them delivered (at-most-once, S59).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_take_pending_notifications(
    handle: u64,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.take_pending_notifications_json(limit)
    })
}

/// Unread inbox messages from VIP senders. `account_ids_json` is a JSON
/// array of account ids, or NULL for every account. Writes the count to
/// `*out_count`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_vip_unread_count(
    handle: u64,
    account_ids_json: *const c_char,
    out_count: *mut i64,
) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_count.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        let account_ids: Option<Vec<String>> = match unsafe { cstr(account_ids_json) } {
            None => {
                if !account_ids_json.is_null() {
                    return CastellumStatus::InvalidArgument;
                }
                None
            }
            Some(json) => match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(_) => return CastellumStatus::InvalidArgument,
            },
        };
        let profile = {
            let map = registry_lock();
            match map.get(&handle) {
                Some(p) => Arc::clone(p),
                None => return CastellumStatus::InvalidHandle,
            }
        };
        match profile.vip_unread_count(account_ids) {
            Ok(n) => {
                unsafe { *out_count = n };
                CastellumStatus::Ok
            }
            Err(e) => e.status(),
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Enable (`enabled != 0`) or disable writes for an account. Disabling returns
/// the account to evaluation (read-only) mode. Enabling is the explicit human
/// act that permits Castellum to change the mail server; the caller must have
/// confirmed the consequences. Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_account_write_enabled(
    handle: u64,
    account_id: *const c_char,
    enabled: i32,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.set_account_write_enabled(&account_id, enabled != 0)
    })
}

/// Write whether the account may change the mail server to `out_enabled`
/// (1 = writes enabled, 0 = evaluation/read-only). Fail-closed: an unknown
/// account reads 0. Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_account_write_enabled(
    handle: u64,
    account_id: *const c_char,
    out_enabled: *mut i32,
) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_enabled.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        let Some(account_id) = (unsafe { cstr(account_id) }) else {
            return CastellumStatus::InvalidArgument;
        };
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        match registry_lock().get(&handle) {
            Some(profile) => match profile.account_write_enabled(&account_id) {
                Ok(v) => {
                    unsafe { *out_enabled = i32::from(v) };
                    CastellumStatus::Ok
                }
                Err(e) => e.status(),
            },
            None => CastellumStatus::InvalidHandle,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Persist the CoreSpotlight indexing opt-in. Profile-
/// wide, not per-account — one `app_meta` row. `enabled != 0` turns
/// on-device indexing of message subject/sender metadata on; the caller
/// (Swift) drives the resulting one-off backfill or domain-scoped
/// `CSSearchableIndex` removal — this call only persists the flag. Returns a
/// CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_spotlight_indexing_enabled(handle: u64, enabled: i32) -> i32 {
    profile_action(handle, |p| p.set_spotlight_indexing_enabled(enabled != 0))
}

/// Write the CoreSpotlight indexing opt-in to `out_enabled` (1 = on, 0 =
/// off). Fail-closed: an unknown/never-opened-with-this-flag profile reads 0.
/// Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_spotlight_indexing_enabled(handle: u64, out_enabled: *mut i32) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_enabled.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        match registry_lock().get(&handle) {
            Some(profile) => match profile.spotlight_indexing_enabled() {
                Ok(v) => {
                    unsafe { *out_enabled = i32::from(v) };
                    CastellumStatus::Ok
                }
                Err(e) => e.status(),
            },
            None => CastellumStatus::InvalidHandle,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Archive a message (move to the account's Archive mailbox).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_archive_message(handle: u64, message_id: *const c_char) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.archive_message(&message_id))
}

/// Delete a message (move to the account's Trash mailbox; undoable).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_message(handle: u64, message_id: *const c_char) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_message(&message_id))
}

/// Archive, trash or move a batch of messages, returning undo handles.
/// `request_json` = `{"message_ids":["…"],"target":"archive"|"trash"|"mailbox",
/// "mailbox_id":"…"}`. Writes `{"operation_ids":[…],"refused":[…]}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_move_messages(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request_json) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.move_messages_json(&request_json))
}

/// Queue the inverse of earlier moves (`{"operation_ids":[…]}`), atomically for
/// the group. Writes `{"queued":n,"account_ids":[…],"source_mailbox_ids":[…],
/// "not_undoable":[…]}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_undo_moves(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request_json) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.undo_moves_json(&request_json))
}

/// Permanently delete ONE message that is already in Trash (destructive;
/// queued through the operation queue). Refused for any message outside
/// Trash. The caller must have shown an explicit confirmation first.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_message_permanently(
    handle: u64,
    message_id: *const c_char,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_message_permanently(&message_id))
}

/// Permanently empty the account's Trash (destructive).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_empty_trash(handle: u64, account_id: *const c_char) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.empty_trash(&account_id).map(|_| ()))
}

/// Create or update a compose draft from `draft_json`; writes its id to
/// `*out_id`. Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_save_draft(
    handle: u64,
    draft_json: *const c_char,
    out_id: *mut i64,
) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if draft_json.is_null() || out_id.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        let Some(draft_json) = (unsafe { cstr(draft_json) }) else {
            return CastellumStatus::InvalidArgument;
        };
        let profile = {
            let map = registry_lock();
            match map.get(&handle) {
                Some(p) => Arc::clone(p),
                None => return CastellumStatus::InvalidHandle,
            }
        };
        match profile.save_draft_json(&draft_json) {
            Ok(id) => {
                unsafe { *out_id = id };
                CastellumStatus::Ok
            }
            Err(e) => {
                set_last_error(e.to_string());
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// All drafts for `account_id` (any status, newest first) as a JSON array.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_drafts(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.list_drafts_json(&account_id))
}

/// Delete a draft (refused once it has left `draft` status).
#[no_mangle]
pub extern "C" fn castellum_delete_draft(handle: u64, draft_id: i64) -> i32 {
    profile_action(handle, |p| p.delete_draft(draft_id))
}

/// Build RFC822 bytes for `draft_id` and enqueue a server-draft save
/// (`server_draft_save` replay op). `from_json` = `{"from_email": str,
/// "from_name": str?}`. Writes `{"queued": bool}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_save_draft_to_server(
    handle: u64,
    draft_id: i64,
    from_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(from_json) = (unsafe { cstr(from_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.save_draft_to_server_json(draft_id, &from_json)
    })
}

/// Hydrate (or reuse) a local editable draft from an already-synced message
/// in a Drafts-role mailbox. Writes the new/existing draft id to `*out_id`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_open_server_draft(
    handle: u64,
    message_id: *const c_char,
    out_id: *mut i64,
) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if message_id.is_null() || out_id.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        let Some(message_id) = (unsafe { cstr(message_id) }) else {
            return CastellumStatus::InvalidArgument;
        };
        let profile = {
            let map = registry_lock();
            match map.get(&handle) {
                Some(p) => Arc::clone(p),
                None => return CastellumStatus::InvalidHandle,
            }
        };
        match profile.open_server_draft_json(&message_id) {
            Ok(id) => {
                unsafe { *out_id = id };
                CastellumStatus::Ok
            }
            Err(e) => {
                set_last_error(e.to_string());
                e.status()
            }
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Queue a draft for delivery: status -> outbox + one replayable `send` op.
#[no_mangle]
pub extern "C" fn castellum_send_draft(handle: u64, draft_id: i64) -> i32 {
    profile_action(handle, |p| p.send_draft(draft_id))
}

/// Queue a draft for delivery after `delay_seconds` (S25 undo-send window;
/// `delay_seconds <= 0` behaves exactly like `castellum_send_draft`): status
/// -> outbox + one 'send' op whose earliest-send time is durable. The sender
/// skips it until that time passes.
#[no_mangle]
pub extern "C" fn castellum_send_draft_delayed(
    handle: u64,
    draft_id: i64,
    delay_seconds: i64,
) -> i32 {
    profile_action(handle, |p| p.send_draft_delayed(draft_id, delay_seconds))
}

/// Send Later (HP1): queue a draft for delivery at `send_at` (Unix epoch
/// seconds, UTC; must be in the future). Same durable 'send' op as the
/// undo-send window, flagged scheduled: it survives quit and relaunch.
#[no_mangle]
pub extern "C" fn castellum_schedule_send(handle: u64, draft_id: i64, send_at: i64) -> i32 {
    profile_action(handle, |p| p.schedule_send(draft_id, send_at))
}

/// Move a waiting (or needs-confirmation) scheduled send to `send_at`.
/// Writes `{"rescheduled": bool}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_reschedule_send(
    handle: u64,
    draft_id: i64,
    send_at: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.reschedule_send_json(draft_id, send_at)
    })
}

/// Cancel a scheduled send; its draft returns, intact, to Drafts. Writes
/// `{"cancelled": bool}` to `*out_json` (`false` = the sender already claimed it).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_cancel_scheduled_send(
    handle: u64,
    draft_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.cancel_scheduled_send_json(draft_id))
}

/// Undo a not-yet-submitted queued send (S25): if the sender has not already
/// claimed it, atomically cancels its 'send' op and returns the draft to
/// `draft` status. Writes `{"in_time": bool}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_undo_send(
    handle: u64,
    draft_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.undo_send_json(draft_id))
}

/// Release a `held` send (crash-safety hold, S25) for immediate delivery on
/// the next drain. Writes `{"released": bool}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_send_now(
    handle: u64,
    draft_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.send_now_json(draft_id))
}

/// Crash-safety startup pass (S25 — see DECISIONS.md): holds any queued
/// `send` op for `account_id` whose undo window cannot be proven to have
/// stayed open under a continuously-live app. Call ONCE per account at
/// launch, before scheduling any drain. Writes `{"held": n}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_reconcile_undo_sends(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.reconcile_undo_sends_json(&account_id)
    })
}

/// Deliver queued (outbox) drafts over SMTP. `config_json` =
/// {account_id, from_email, from_name?, smtp:{host, port, username, password,
/// use_tls, allow_invalid_certs}}. Writes a JSON SendReport to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_send_pending(
    handle: u64,
    config_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        clear_last_error();
        set_last_error("config_json must be non-null valid UTF-8".to_string());
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.send_pending_json(&config_json))
}

/// OB1: every message waiting in the Outbox for `account_id` (drafts in
/// `outbox` status with their send state) as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_outbox_sends(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.outbox_sends_json(&account_id))
}

/// OB1 Outbox "Send Now": attempt exactly `draft_id` now over SMTP
/// (`config_json` as for `castellum_send_pending`). Writes a JSON SendReport
/// for that one message to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_send_draft_now(
    handle: u64,
    config_json: *const c_char,
    draft_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        clear_last_error();
        set_last_error("config_json must be non-null valid UTF-8".to_string());
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.send_draft_now_json(&config_json, draft_id)
    })
}

/// OB1 Outbox "Send All": requeue every failed send of `account_id` with a
/// fresh attempt budget. Writes `{"requeued": n}` to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_requeue_failed_sends(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.requeue_failed_sends_json(&account_id)
    })
}

/// Pending write-path operations for `account_id` as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_pending_operations(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.pending_operations_json(&account_id))
}

/// Manually retry a failed/failed_permanent operation (outbox "Retry"). Any
/// other current status is a benign no-op; unknown ids error. The next
/// replay pass picks the re-queued op up.
#[no_mangle]
pub extern "C" fn castellum_retry_operation(handle: u64, op_id: i64) -> i32 {
    profile_action(handle, |p| p.retry_operation(op_id).map(|_| ()))
}

/// Manually cancel an undelivered operation (outbox "Cancel"). Running ops
/// are refused (benign no-op); unknown ids error. Terminal: the op's large
/// payload is reclaimed.
#[no_mangle]
pub extern "C" fn castellum_cancel_operation(handle: u64, op_id: i64) -> i32 {
    profile_action(handle, |p| p.cancel_operation(op_id).map(|_| ()))
}

/// Replay queued write-path operations to the IMAP server. `config_json` carries
/// the account_id + credentials; writes a JSON ReplayReport to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_replay_operations(
    handle: u64,
    config_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.replay_operations_json(&config_json))
}

/// List message headers (newest first) for a mailbox as a JSON array.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_messages(
    handle: u64,
    account_id: *const c_char,
    mailbox_id: *const c_char,
    limit: i64,
    offset: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(account_id), Some(mailbox_id)) =
        (unsafe { cstr(account_id) }, unsafe { cstr(mailbox_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.list_messages_json(&account_id, &mailbox_id, limit, offset)
    })
}

/// Keyset message-list paging (S64). `cursor_json` is null for the first
/// page or the "next" object from the previous page; writes
/// `{"rows":[MessageHeader…],"next":{"sent_at":…,"id":…}|null}`. Same
/// ordering and snooze exclusion as `castellum_list_messages`; cost is
/// independent of how deep the page is.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_messages_page(
    handle: u64,
    account_id: *const c_char,
    mailbox_id: *const c_char,
    cursor_json: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(account_id), Some(mailbox_id)) =
        (unsafe { cstr(account_id) }, unsafe { cstr(mailbox_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    let cursor_json = unsafe { cstr(cursor_json) };
    profile_json(handle, out_json, |p| {
        p.list_messages_page_json(&account_id, &mailbox_id, cursor_json.as_deref(), limit)
    })
}

/// Same shape as [`castellum_list_messages_page`], scoped by mailbox role
/// across every account (the unified-inbox list).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_messages_by_role_page(
    handle: u64,
    role: *const c_char,
    cursor_json: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(role) = (unsafe { cstr(role) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let cursor_json = unsafe { cstr(cursor_json) };
    profile_json(handle, out_json, |p| {
        p.list_messages_by_role_page_json(&role, cursor_json.as_deref(), limit)
    })
}

/// Sorted keyset paging (list-sort). `sort_key` is "date", "from",
/// "subject", "unread", "starred" or "attachments" (NULL or "" = "date"); `ascending` is 0 or 1 (with "date", 0 is
/// newest first, the order of `castellum_list_messages_page`). Rows without a
/// date are always last. `cursor_json` is NULL for the first page, else the
/// "next" object of the previous page FOR THE SAME sort_key and ascending; a
/// cursor from another sort, or an unknown sort_key, is InvalidArgument.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_messages_page_sorted(
    handle: u64,
    account_id: *const c_char,
    mailbox_id: *const c_char,
    sort_key: *const c_char,
    ascending: i32,
    cursor_json: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(account_id), Some(mailbox_id)) =
        (unsafe { cstr(account_id) }, unsafe { cstr(mailbox_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    let sort_key = unsafe { cstr(sort_key) };
    let cursor_json = unsafe { cstr(cursor_json) };
    profile_json(handle, out_json, |p| {
        p.list_messages_page_sorted_json(
            &account_id,
            &mailbox_id,
            sort_key.as_deref(),
            ascending != 0,
            cursor_json.as_deref(),
            limit,
        )
    })
}

/// [`castellum_list_messages_page_sorted`] scoped by mailbox role across
/// every account (the unified-inbox list).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_messages_by_role_page_sorted(
    handle: u64,
    role: *const c_char,
    sort_key: *const c_char,
    ascending: i32,
    cursor_json: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(role) = (unsafe { cstr(role) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let sort_key = unsafe { cstr(sort_key) };
    let cursor_json = unsafe { cstr(cursor_json) };
    profile_json(handle, out_json, |p| {
        p.list_messages_by_role_page_sorted_json(
            &role,
            sort_key.as_deref(),
            ascending != 0,
            cursor_json.as_deref(),
            limit,
        )
    })
}

/// Fetch a full message by id as JSON (`null` if not found) in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_get_message(
    handle: u64,
    message_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.get_message_json(&message_id))
}

/// Perform the RFC 8058 one-click unsubscribe POST for `message_id`. Writes
/// `{"status":"sent","http_status":n}`, `{"status":"unavailable"}` (the
/// message has no one-click target), or an error (`CastellumStatus::Network`
/// on any transport/non-2xx failure) to `*out_json`. NEVER call this except
/// after the caller has already shown the user the exact destination and
/// they confirmed — this function itself performs no confirmation.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_unsubscribe_one_click(
    handle: u64,
    message_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.unsubscribe_one_click_json(&message_id)
    })
}

/// Search within `account_id` as a JSON array of headers. `query` accepts
/// the S39 search grammar (from:/to:/cc:/subject:/has:attachment/
/// before:/after:/in:/is:, `-` negation, quoted phrases) as well as bare
/// keywords — unchanged signature, see `castellum_db::search_query`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_search_messages(
    handle: u64,
    account_id: *const c_char,
    query: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(account_id), Some(query)) = (unsafe { cstr(account_id) }, unsafe { cstr(query) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.search_messages_json(&account_id, &query, limit)
    })
}

/// List thread summaries for `account_id` as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_threads(
    handle: u64,
    account_id: *const c_char,
    limit: i64,
    offset: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.list_threads_json(&account_id, limit, offset)
    })
}

/// List a thread's message headers (oldest first) as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_get_thread(
    handle: u64,
    thread_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(thread_id) = (unsafe { cstr(thread_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.get_thread_json(&thread_id))
}

/// List message headers across EVERY account whose mailbox has `role` (e.g.
/// "inbox"), newest first, paginated — the S34 unified inbox.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_messages_by_role(
    handle: u64,
    role: *const c_char,
    limit: i64,
    offset: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(role) = (unsafe { cstr(role) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.list_messages_by_role_json(&role, limit, offset)
    })
}

/// List thread summaries across every account (newest activity first,
/// paginated) as a JSON array in `*out_json`. Not yet wired to a view.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_threads_across_accounts(
    handle: u64,
    limit: i64,
    offset: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.list_threads_across_accounts_json(limit, offset)
    })
}

/// Search across every account as a JSON array of headers in `*out_json` —
/// the S34 unified counterpart to `castellum_search_messages`. Same S39
/// grammar, unchanged signature.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_search_messages_across_accounts(
    handle: u64,
    query: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(query) = (unsafe { cstr(query) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.search_messages_across_accounts_json(&query, limit)
    })
}

// --- AI job queue (slice 7b) -------------------------------------------------

/// Enqueue an AI job; `request_json` = JSON with feature + refs. Writes
/// `{"job_id": n}` to `*out_json`. Returns a [`CastellumStatus`] code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_enqueue_ai_job(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request_json) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.enqueue_ai_job_json(&request_json))
}

/// Run pending AI jobs until the queue is drained; `config_json` carries the
/// endpoint and model. Writes an `AiRunReport` JSON to `*out_json`. Blocking.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_run_ai_jobs(
    handle: u64,
    config_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.run_ai_jobs_json(&config_json))
}

/// Poll a job's current state and any new events after `after_event_id`.
/// Writes `{"job":{...}, "events":[...]}` or `null` if the job is unknown.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_poll_ai_job(
    handle: u64,
    job_id: i64,
    after_event_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.poll_ai_job_json(job_id, after_event_id)
    })
}

/// "Find similar" (S53-UI): embed `message_id` (ad-hoc, reusing a stored
/// embedding when one exists), then find its nearest neighbours by cosine
/// similarity. `config_json` is the same shape `castellum_run_ai_jobs`
/// takes. `account_id` is NULLABLE (null = every account). Writes
/// `{"results":[{"header":MessageHeader,"similarity":f32}, ...]}` to
/// `*out_json`. Invoked only from an explicit "Find similar" gesture — never
/// on the typing path (see `castellum_search_messages`/
/// `castellum_search_messages_across_accounts`, whose signatures this stage
/// does not touch). May take longer than other search calls (a network
/// round trip to Ollama on the ad-hoc-embed path) — the caller MUST invoke
/// this off the main thread.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_find_similar_messages(
    handle: u64,
    config_json: *const c_char,
    message_id: *const c_char,
    account_id: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(config_json), Some(message_id)) =
        (unsafe { cstr(config_json) }, unsafe { cstr(message_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    // account_id is nullable: null means "every account".
    let account_id = unsafe { cstr(account_id) };
    profile_json(handle, out_json, |p| {
        p.find_similar_json(&config_json, &message_id, account_id.as_deref(), limit)
    })
}

/// Request cooperative cancellation of `job_id`. Queued jobs are cancelled
/// immediately; in-flight jobs set a flag that the executor checks between
/// token flushes. Returns [`CastellumStatus::Ok`] even if already terminal.
#[no_mangle]
pub extern "C" fn castellum_cancel_ai_job(handle: u64, job_id: i64) -> i32 {
    profile_action(handle, |p| p.cancel_ai_job(job_id))
}

/// List models available at the Ollama endpoint. `config_json` must contain
/// `"endpoint"`. Writes a JSON array of `{name, size_bytes}` to `*out_json`.
/// The egress guard in the provider fires here — non-private endpoints are
/// refused.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_ai_list_models(
    handle: u64,
    config_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(config_json) = (unsafe { cstr(config_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.ai_list_models_json(&config_json))
}

/// Retrieve a cached AI output by its natural key `(scope, owner_id, feature)`.
/// Writes the `AiOutput` JSON to `*out_json`, or the string `"null"` when not
/// found. Returns [`CastellumStatus::Ok`] in both cases.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_get_ai_output(
    handle: u64,
    scope: *const c_char,
    owner_id: *const c_char,
    feature: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(scope), Some(owner_id), Some(feature)) =
        (unsafe { cstr(scope) }, unsafe { cstr(owner_id) }, unsafe {
            cstr(feature)
        })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.get_ai_output_json(&scope, &owner_id, &feature)
            .map(|opt| opt.unwrap_or_else(|| "null".to_string()))
    })
}

/// Retrieve a thread's typed extraction rows (`extract_thread_items`, S54).
/// Writes a JSON array to `*out_json` — empty (`[]`) for a thread with
/// nothing extracted yet, never an error on that account.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_get_thread_extractions(
    handle: u64,
    thread_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(thread_id) = (unsafe { cstr(thread_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.get_thread_extractions_json(&thread_id)
    })
}

// --- contacts (slice 8a) ----------------------------------------------------

/// Recompute the contacts aggregate + co-occurrence edge graph from synced
/// mail. Writes a JSON AggregateReport {contacts_upserted, edges_rebuilt,
/// elapsed_ms} to `*out_json`. User-owned contact fields are never touched.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_refresh_contacts(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.refresh_contacts_json())
}

/// One-off repair of subjects and display names that a build before stage X1
/// stored RFC 2047-encoded (`=?UTF-8?Q?...?=`): decodes them, re-indexes the
/// rows for search and re-threads. Bounded batches, resumable, and a no-op
/// once it has completed for the profile. Call it off the launch path. Writes
/// a JSON EnvelopeTextRepairReport {already_done, batches, scanned,
/// messages_repaired, subjects_repaired, names_repaired} to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_repair_encoded_envelope_text(
    handle: u64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.repair_encoded_envelope_text_json())
}

/// List contacts (most recent first) as a JSON array in `*out_json`. `query`
/// is NULLABLE (null/blank = no filter; otherwise substring-matches
/// email/derived_name/display_name). `include_hidden != 0` includes hidden
/// contacts.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_contacts(
    handle: u64,
    query: *const c_char,
    include_hidden: i32,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    // Null query is a valid "no filter" (unlike the other id arguments); an
    // invalid-UTF-8 query also degrades to no filter rather than erroring.
    let query = unsafe { cstr(query) };
    profile_json(handle, out_json, |p| {
        p.list_contacts_json(query.as_deref(), include_hidden != 0, limit)
    })
}

/// Recipients of sent/queued mail (newest first, at most `limit`) as a JSON
/// array in `*out_json` -- the local-only address-autocomplete source.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_sent_recipients(
    handle: u64,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.list_sent_recipients_json(limit))
}

/// One contact by id as JSON (`null` if not found) in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_get_contact(
    handle: u64,
    contact_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.get_contact_json(contact_id))
}

/// Update a contact's USER-OWNED fields from `{id, display_name?, notes?,
/// is_hidden?}` JSON. Derived columns are refresh-owned and unreachable here.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_update_contact(handle: u64, json: *const c_char) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.update_contact_json(&json))
}

/// Create a contact by hand from `{email, name?}` JSON (is_user_created = 1);
/// writes the stored contact as JSON to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_create_contact(
    handle: u64,
    json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.create_contact_json(&json))
}

// --- account identities (S47) -----------------------------------------------

/// List `account_id`'s extra identities (aliases) as a JSON array in
/// `*out_json`. The account's own address is not included — see migration
/// 0026's doc comment.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_identities(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(account_id) = (unsafe { cstr(account_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.list_identities_json(&account_id))
}

/// Create an identity from `{account_id, email, display_name?}` JSON;
/// writes the stored row to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_create_identity(
    handle: u64,
    json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.create_identity_json(&json))
}

/// Rename an identity's display name from `{id, display_name?}` JSON.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_update_identity(handle: u64, json: *const c_char) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.update_identity_json(&json))
}

/// Delete an identity by id.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_identity(handle: u64, identity_id: i64) -> i32 {
    profile_action(handle, |p| p.delete_identity(identity_id))
}

// --- contact enrichment (slice 8d) ------------------------------------------

/// JSON-dispatch endpoint for contact enrichment operations.
///
/// `op_json` must be one of:
///   `{"op":"list_sources"}` → JSON array of `{id, display_name, kind, input_label}`.
///   `{"op":"enrich","contact_id":N,"source_id":"manual","input":"...","save":true}` →
///     JSON array of enrichment fields (preview if `save=false`, persisted if `save=true`;
///     the two modes return DIFFERENT shapes — see `Profile::enrich_contact_json`).
///   `{"op":"delete","id":N}` → `"{}"` on success.
///   `{"op":"raw_add","contact_id":N,"source":"manual","field":"...","value":"...","provenance":null}` →
///     `"{}"` on success (direct upsert, bypasses the parser).
///
/// Writes the result JSON to `*out_json` (free with `castellum_string_free`).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_enrichment(
    handle: u64,
    op_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(op_json) = (unsafe { cstr(op_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.enrichment_dispatch_json(&op_json))
}

// --- projects (slice 8b) ----------------------------------------------------

/// List projects (with email/contact counts) as a JSON array in `*out_json`.
/// `include_archived != 0` includes soft-deleted projects.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_projects(
    handle: u64,
    include_archived: i32,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.list_projects_json(include_archived != 0)
    })
}

/// Create or update a project from `{id?, name, notes?, status?, color?, position?}`
/// JSON; writes the stored project as JSON to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_upsert_project(
    handle: u64,
    json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.upsert_project_json(&json))
}

/// Permanently delete a project (cascade drops link rows). Returns Ok or an error.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_project(handle: u64, project_id: *const c_char) -> i32 {
    let Some(project_id) = (unsafe { cstr(project_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_project(&project_id))
}

/// Fetch a project with linked email headers and contacts as JSON in `*out_json`
/// (`null` when not found).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_get_project(
    handle: u64,
    project_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(project_id) = (unsafe { cstr(project_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.get_project_json(&project_id))
}

/// Link (`linked != 0`) or unlink a message from a project. `auto_contacts != 0`
/// inserts non-self, non-hidden from/to/cc participants as `origin='auto'`
/// contacts; never downgrades an existing `'manual'` row.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_project_set_email_link(
    handle: u64,
    project_id: *const c_char,
    message_id: *const c_char,
    linked: i32,
    auto_contacts: i32,
) -> i32 {
    let (Some(project_id), Some(message_id)) =
        (unsafe { cstr(project_id) }, unsafe { cstr(message_id) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.project_set_email_link(&project_id, &message_id, linked != 0, auto_contacts != 0)
    })
}

/// Link (`linked != 0`) or unlink a contact from a project (`origin='manual'`).
/// Linking over an `'auto'` row upgrades it to `'manual'`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_project_set_contact_link(
    handle: u64,
    project_id: *const c_char,
    contact_id: i64,
    linked: i32,
) -> i32 {
    let Some(project_id) = (unsafe { cstr(project_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.project_set_contact_link(&project_id, contact_id, linked != 0)
    })
}

/// Projects that contain `message_id` as a JSON array of
/// `{project_id, name, color}` in `*out_json` (reverse lookup).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_projects_for_message(
    handle: u64,
    message_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(message_id) = (unsafe { cstr(message_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.projects_for_message_json(&message_id)
    })
}

/// Move a project to a new Kanban column (`status`) and `position`, re-packing
/// both affected columns atomically.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_move_project(
    handle: u64,
    project_id: *const c_char,
    status: *const c_char,
    position: i64,
) -> i32 {
    let (Some(project_id), Some(status)) = (unsafe { cstr(project_id) }, unsafe { cstr(status) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.move_project(&project_id, &status, position))
}

// --- smart mailboxes (S41) --------------------------------------------------

/// List smart mailboxes (sidebar order) as a JSON array in `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_smart_mailboxes(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.list_smart_mailboxes_json())
}

/// Create (`id` absent/empty) or rename/re-query (`id` present) a smart
/// mailbox from `{id?, name, query?, symbol?, group?, scope?}` JSON (MM-1:
/// `group` = nested all/any/none conditions, `scope` = account/mailbox
/// restriction); writes the stored row (counts freshly recomputed) as JSON to
/// `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_upsert_smart_mailbox(
    handle: u64,
    json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.upsert_smart_mailbox_json(&json))
}

/// Permanently delete a smart mailbox (never touches any mail).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_smart_mailbox(
    handle: u64,
    smart_mailbox_id: *const c_char,
) -> i32 {
    let Some(id) = (unsafe { cstr(smart_mailbox_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_smart_mailbox(&id))
}

/// Reorder from a JSON array of ids, e.g. `["id1","id2","id3"]`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_reorder_smart_mailboxes(handle: u64, json: *const c_char) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.reorder_smart_mailboxes_json(&json))
}

/// Recompute every smart mailbox's cached counts; writes the refreshed list
/// as JSON to `*out_json`. Call after a sync pass, not per keystroke.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_refresh_smart_mailbox_counts(
    handle: u64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.refresh_smart_mailbox_counts_json())
}

/// MM-1c: header-condition policy (`max_name_len`, `builtin_fields`,
/// `not_stored`, `not_stored_prefixes`, `not_stored_substrings`, `known_names`)
/// as JSON. Local-only; free with `castellum_string_free`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_filter_header_policy(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.filter_header_policy_json())
}

/// MM-1c: `{"captured":N,"total":M}` -- messages whose headers are stored vs
/// all messages. Local-only.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_message_header_coverage(
    handle: u64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.message_header_coverage_json())
}

/// Runs one smart mailbox's saved S39-grammar query cross-account (the same
/// path `castellum_search_messages_across_accounts` uses); writes a JSON
/// array of `MessageHeader`, or JSON `null` when `smart_mailbox_id` is
/// unknown (never an error), to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_messages_for_smart_mailbox(
    handle: u64,
    smart_mailbox_id: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(id) = (unsafe { cstr(smart_mailbox_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.messages_for_smart_mailbox_json(&id, limit)
    })
}

// --- Split Inbox (HP-2) -------------------------------------------------------

/// HP-2: exact total/unread of every Inbox split (tab order) then Other
/// (`id == "other"`, always last) over `{"mailbox_ids": [...]}` (empty = every
/// account's Inbox); writes a JSON array of `InboxSplitCount` to `*out_json`.
/// A split is a group smart mailbox whose `scope.inbox_split` is true.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_inbox_split_counts(
    handle: u64,
    json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.inbox_split_counts_json(&json))
}

/// HP-2: one tab's messages from `{"mailbox_ids": [...], "split_id": <id> or
/// "other", "limit"?: n}`, newest first; writes a JSON array of
/// `MessageHeader`, or JSON `null` when the split no longer exists, to
/// `*out_json`. Every Inbox message is in exactly one tab.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_inbox_split_messages(
    handle: u64,
    json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.inbox_split_messages_json(&json))
}

// --- filters / rules (S40) ---------------------------------------------------

/// Create (`id` absent/empty) or update (`id` present) a saved filter from
/// `{id?, account_id?, name, kind, group, sort_field?, sort_desc?, trigger?,
/// actions?}` JSON (a `SavedFilter`-shaped request; `kind`/`trigger`/
/// `actions` use the same snake_case tags `FilterField`/`RuleAction` etc.
/// serialize as). Writes the stored row as JSON to `*out_json`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_upsert_filter(
    handle: u64,
    filter_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(filter_json) = (unsafe { cstr(filter_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.upsert_filter_json(&filter_json))
}

/// MM-3: run order for rules. `json` = a JSON array of filter ids in the
/// order they should run; unknown ids are ignored.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_reorder_filters(handle: u64, json: *const c_char) -> i32 {
    let Some(json) = (unsafe { cstr(json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.reorder_filters_json(&json))
}

/// List saved filters (smart mailboxes and rules) as a JSON array in
/// `*out_json`. `account_id` is NULLABLE: null lists every filter; non-null
/// scopes to that account's own filters plus every account-agnostic one.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_filters(
    handle: u64,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let account_id = unsafe { cstr(account_id) };
    profile_json(handle, out_json, |p| {
        p.list_filters_json(account_id.as_deref())
    })
}

/// One saved filter as JSON, or JSON `null` when `filter_id` is unknown
/// (never an error).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_get_filter(
    handle: u64,
    filter_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(filter_id) = (unsafe { cstr(filter_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.get_filter_json(&filter_id))
}

/// Permanently delete a saved filter. Errors if `filter_id` is unknown.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_filter(handle: u64, filter_id: *const c_char) -> i32 {
    let Some(filter_id) = (unsafe { cstr(filter_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_filter(&filter_id))
}

/// Evaluates `group_json` (a serialized `FilterGroup` — the memo's §6
/// `filter_json_or_id` union is split into this plus `castellum_get_filter`
/// for the "run a saved filter" case, since a C-string union of "JSON object
/// or bare id" is ambiguous at the boundary; see the plan's Deviations log)
/// and writes matching `MessageHeader` rows as JSON in `*out_json`, same
/// shape as `castellum_search_messages`. `account_id` is NULLABLE (null =
/// every account). Does NOT run rule actions — see `castellum_run_rule_now`
/// below. Never touches the operations queue: no
/// call into that insert path exists on this read-only entry point.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_evaluate_filter(
    handle: u64,
    group_json: *const c_char,
    account_id: *const c_char,
    limit: i64,
    offset: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(group_json) = (unsafe { cstr(group_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let account_id = unsafe { cstr(account_id) };
    profile_json(handle, out_json, |p| {
        p.evaluate_filter_json(&group_json, account_id.as_deref(), limit, offset)
    })
}

/// Additive, documented extension to the memo's §6 FFI list (not in its
/// original sketch — added for Task 4's `preview_rule`): previews a saved
/// `kind = 'rule'` filter — matches paired with the actions that WOULD
/// apply, as `{"matches":[...MessageHeader...],"actions":[...]}` JSON —
/// without enqueueing anything. `account_id` is NULLABLE. Errors if
/// `filter_id` is unknown or is not a rule.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_preview_rule(
    handle: u64,
    filter_id: *const c_char,
    account_id: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(filter_id) = (unsafe { cstr(filter_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let account_id = unsafe { cstr(account_id) };
    profile_json(handle, out_json, |p| {
        p.preview_rule_json(&filter_id, account_id.as_deref(), limit)
    })
}

/// MM-3 review: the rule editor's dry run of UNSAVED conditions. Writes
/// `{"matches":[...MessageHeader...],"total":N}` — the first `limit`
/// messages a rule with `group_json` would act on and the exact count, with
/// Trash/Junk/Drafts/Sent left out exactly as the engine does. Read-only.
/// `account_id` is NULLABLE (null = every account).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_preview_rule_conditions(
    handle: u64,
    group_json: *const c_char,
    account_id: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(group_json) = (unsafe { cstr(group_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let account_id = unsafe { cstr(account_id) };
    profile_json(handle, out_json, |p| {
        p.preview_rule_conditions_json(&group_json, account_id.as_deref(), limit)
    })
}

/// MM-3 review: one bounded Run Now batch. Queues the actions of rule
/// `filter_id` for its next `limit` (clamped to 1..=1000) candidates whose
/// message id sorts after `after` (NULLABLE = from the start) and writes the
/// `RuleRunReport` as JSON; its `next_after`, when present, is the `after`
/// for the next batch. The app calls this repeatedly off the main thread,
/// so a large Run Now shows progress and can be cancelled between batches.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_run_rule_now_batch(
    handle: u64,
    filter_id: *const c_char,
    account_id: *const c_char,
    after: *const c_char,
    limit: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(filter_id) = (unsafe { cstr(filter_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let account_id = unsafe { cstr(account_id) };
    let after = unsafe { cstr(after) };
    profile_json(handle, out_json, |p| {
        p.run_rule_now_batch_json(&filter_id, account_id.as_deref(), after.as_deref(), limit)
    })
}

/// The "Run now" / `Manual` path (Assumption 4) in one call: runs exactly
/// one rule (regardless of its own `trigger_kind`) against its current
/// scope, at most 10,000 messages (`next_after` in the report says where a
/// further run would continue), and writes the `RuleRunReport` as JSON.
/// `account_id` is NULLABLE (falls back to the filter's own stored
/// `account_id`). The app uses `castellum_run_rule_now_batch` instead.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_run_rule_now(
    handle: u64,
    filter_id: *const c_char,
    account_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(filter_id) = (unsafe { cstr(filter_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    let account_id = unsafe { cstr(account_id) };
    profile_json(handle, out_json, |p| {
        p.run_rule_now_json(&filter_id, account_id.as_deref())
    })
}

// --- people graph (slice 8c) -------------------------------------------------

/// Build the top-N contact subgraph and return it as JSON in `*out_json`.
/// `project_id` is NULLABLE (null = all contacts; non-null restricts to project
/// members). `max_nodes` is clamped to [1..200]. Returns a CastellumStatus code.
/// Output shape: `{"nodes":[{id,label,message_count}],"edges":[{a,b,weight}]}`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_contact_graph(
    handle: u64,
    project_id: *const c_char,
    max_nodes: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    // project_id is nullable: null means "no filter".
    let project_id = unsafe { cstr(project_id) };
    profile_json(handle, out_json, |p| {
        p.contact_graph_json(castellum_core::graph::GraphParams {
            project_id: project_id.as_deref(),
            max_nodes,
        })
    })
}

/// Generate the LOCAL-ONLY diagnostics view as JSON in `*out_json`. Includes
/// real account emails and mailbox paths — MUST NEVER be written to an exported
/// support bundle or sent off-device. Strictly read-only. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_local_diagnostics(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.local_diagnostics_json())
}

/// Idempotently create/refresh a link in the shared evidence-backed link
/// model. `request_json` shape: `{src_kind, src_id, dst_kind, dst_id,
/// relation, origin?, provenance?, confidence?}` (origin defaults to
/// 'manual'). Writes the stored LinkRecord JSON to `*out_json`. Manual rows
/// are never downgraded; derived refreshes never touch the user note. Free
/// with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_upsert_link(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.upsert_link_json(&request))
}

/// Remove one link named by `{src_kind, src_id, dst_kind, dst_id, relation}`.
/// Idempotent: removing a missing link succeeds.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_unlink(handle: u64, request_json: *const c_char) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.unlink_json(&request).map(|_| ()))
}

/// Set or clear the user-owned note on an existing link. `request_json`
/// shape: `{src_kind, src_id, dst_kind, dst_id, relation, note?}` (note
/// null/absent clears). Errors when the link does not exist.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_link_note(handle: u64, request_json: *const c_char) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.set_link_note_json(&request))
}

/// Every link touching `(kind, id)` in either direction, newest activity
/// first, as a JSON array of LinkRecord — origin, provenance ("why is this
/// connected?"), confidence, note, timestamps. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_links_for(
    handle: u64,
    kind: *const c_char,
    id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(kind), Some(id)) = (unsafe { cstr(kind) }, unsafe { cstr(id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.links_for_json(&kind, &id))
}

/// Create or update a work item (Active Work). `request_json` shape:
/// `{id?, project_id?, title, notes?, state?, position?, due_at?,
/// delegated_to_contact_id?}`; state is one of
/// needs_reply|waiting|delegated|blocked|done|someday (default needs_reply).
/// `waiting_since`/`completed_at` are set by the core on state transitions.
/// Omitted `due_at`/`delegated_to_contact_id` keep their value; `null` clears.
/// Writes the stored WorkItem JSON to `*out_json`. Work items are NEVER
/// created automatically from mail. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_upsert_work_item(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.upsert_work_item_json(&request))
}

/// A project's work items in deterministic order (state lane, position,
/// created_at, id) as a JSON array of WorkItem. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_work_items(
    handle: u64,
    project_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(project_id) = (unsafe { cstr(project_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.list_work_items_json(&project_id))
}

/// Filtered work items (Active Work) as a JSON array of WorkItem, ordered by
/// due date ascending (undated last), then position, then id. `filter_json`
/// keys, all optional: `project_id`, `states` (array), `due_before`,
/// `due_after` (epoch seconds), `overdue`, `contact_id`, `linked_message_id`,
/// `linked_thread_id`, `unassigned`, `due` (array of `today`/`this_week`/
/// `overdue`, OR-ed), `waiting_on_me`, `waiting_on_others`. An unknown key is
/// an error, not ignored.
/// Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_work_items_filtered(
    handle: u64,
    filter_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(filter) = (unsafe { cstr(filter_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.list_work_items_filtered_json(&filter)
    })
}

/// Reverse lookup: the work items linked to a message, thread or contact.
/// `request_json` = `{"kind":"message"|"thread"|"contact","id":"..."}`
/// (a contact also matches items delegated to it). Writes a JSON array of
/// WorkItem. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_work_items_for(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.work_items_for_json(&request))
}

/// Suggestions you accept (AW2) as a JSON array of Suggestion, newest first.
/// `filter_json` keys, all optional: `status` (proposed|accepted|dismissed),
/// `kind` (work_item|waiting_for_reply|state_change), `thread_id`,
/// `project_id`, `limit`. A NULL or empty filter lists everything; an unknown
/// key is an error. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_suggestions(
    handle: u64,
    filter_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let filter = unsafe { cstr(filter_json) }.unwrap_or_default();
    profile_json(handle, out_json, |p| p.list_suggestions_json(&filter))
}

/// Accept a suggestion: creates the work item (or applies the state change)
/// and its evidence links in one transaction. Idempotent. Writes a
/// SuggestionAccepted object `{suggestion, work_item}`. Accepting a dismissed
/// suggestion is an error. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_accept_suggestion(
    handle: u64,
    id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(id) = (unsafe { cstr(id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.accept_suggestion_json(&id))
}

/// Dismiss a suggestion (idempotent; its dedupe key is never proposed again).
/// Writes the Suggestion. Dismissing an accepted one is an error. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_dismiss_suggestion(
    handle: u64,
    id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(id) = (unsafe { cstr(id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.dismiss_suggestion_json(&id))
}

/// Number of suggestions still waiting for a decision, as `{"proposed": n}`
/// (for a badge). Free with [`castellum_string_free`].
#[no_mangle]
pub extern "C" fn castellum_count_suggestions(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.count_suggestions_json())
}

/// Permanently delete a work item; its shared-link rows cascade.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_work_item(handle: u64, id: *const c_char) -> i32 {
    let Some(id) = (unsafe { cstr(id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_work_item(&id))
}

// ── Work-item provenance (migration 0050) ───────────────────────────────────

/// A work item's provenance as a JSON array of `{work_item_id, source_kind,
/// source_id, created_at}`, oldest first. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_work_item_provenance(
    handle: u64,
    work_item_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(id) = (unsafe { cstr(work_item_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.list_work_item_provenance_json(&id))
}

/// Record a provenance row. `request_json` = `{work_item_id, source_kind:
/// message|suggestion|manual, source_id?}`. Idempotent; writes the stored
/// row. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_record_work_item_provenance(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.record_work_item_provenance_json(&request)
    })
}

/// Reverse lookup: a JSON array of work-item ids that came from a source.
/// `request_json` = `{source_kind, source_id}`. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_work_items_from_source(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.work_items_from_source_json(&request)
    })
}

// ── Project activity + decision timeline (migration 0049) ──────────────────

/// Append an `activity` or `decision` entry to a project's append-only
/// timeline. `request_json` = `{project_id, kind, summary, ref_kind?,
/// ref_id?}`. Writes the stored TimelineEntry JSON. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_add_timeline_entry(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.add_timeline_entry_json(&request))
}

/// Record a decision on a project's timeline. `request_json` =
/// `{project_id, summary, ref_kind?, ref_id?}`. Writes the stored
/// TimelineEntry JSON. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_record_decision(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.record_decision_json(&request))
}

/// One page of a project's timeline, newest first, as a JSON array of
/// TimelineEntry. `request_json` = `{project_id, kinds?, limit?, offset?}`.
/// Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_timeline(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.list_timeline_json(&request))
}

// ── Saved Active Work views (AW3) ───────────────────────────────────────────

/// Saved views in display order as a JSON array of SavedView `{id, name,
/// filter, position, built_in, hidden, created_at, updated_at}`. The four
/// built-ins are seeded on first use. `include_hidden` != 0 also lists hidden
/// views. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_saved_views(
    handle: u64,
    include_hidden: i32,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.list_saved_views_json(include_hidden != 0)
    })
}

/// Create a custom saved view. `request_json` = `{"name":"..","filter":{..}}`
/// (filter = the work-item filter schema; an invalid filter is an error and
/// nothing is stored). Writes the stored SavedView. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_create_saved_view(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.create_saved_view_json(&request))
}

/// Rename, re-filter and/or hide a view. `request_json` = `{"id":"..",
/// "name"?,"filter"?,"hidden"?}`; a built-in accepts only `hidden`. Writes
/// the stored SavedView. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_update_saved_view(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(request) = (unsafe { cstr(request_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.update_saved_view_json(&request))
}

/// Delete a custom saved view; a built-in is refused (hide it instead).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_delete_saved_view(handle: u64, id: *const c_char) -> i32 {
    let Some(id) = (unsafe { cstr(id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| p.delete_saved_view(&id))
}

/// Reorder saved views. `ids_json` is a JSON array of view ids: the listed
/// ones come first in that order, unknown ids are ignored and unlisted views
/// keep their relative order after them. Writes every view (hidden included)
/// in the new order. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_reorder_saved_views(
    handle: u64,
    ids_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(ids) = (unsafe { cstr(ids_json) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.reorder_saved_views_json(&ids))
}

/// The item count of every visible saved view in ONE call, in display order:
/// a JSON array `[{"id":"..","count":N}]`. Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_saved_view_counts(handle: u64, out_json: *mut *mut c_char) -> i32 {
    profile_json(handle, out_json, |p| p.saved_view_counts_json())
}

/// The work items of one saved view as a JSON array of WorkItem, ordered like
/// `castellum_list_work_items_filtered` (same evaluator). Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_saved_view_items(
    handle: u64,
    id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(id) = (unsafe { cstr(id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.saved_view_items_json(&id))
}

/// Unassigned mail: one page of inbox threads that have neither a project
/// link nor a work-item link, newest first. `request_json` = `{"limit"?
/// (default 50, max 500),"offset"?}` (null or empty = defaults). Writes
/// `{"threads":[{"thread_id","message_count","last_message_at"}],"total",
/// "limit","offset"}` — ids and counts only. Free with
/// [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_unassigned_mail(
    handle: u64,
    request_json: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let request = unsafe { cstr(request_json) }.unwrap_or_default();
    profile_json(handle, out_json, |p| p.unassigned_mail_json(&request))
}

/// Explainable communication-health signals for one contact: a JSON object
/// `{subject_kind,subject_id,as_of,signals:[{key,value|null,unit,explanation,
/// evidence_kind,evidence_ids}]}`, or the string `null` for an unknown contact.
/// Evidence is ids only (no bodies). Free with [`castellum_string_free`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_contact_health(
    handle: u64,
    contact_id: i64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| p.contact_health_json(contact_id))
}

/// Same as [`castellum_contact_health`] for one thread id.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_thread_health(
    handle: u64,
    thread_id: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let Some(thread_id) = (unsafe { cstr(thread_id) }) else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| p.thread_health_json(&thread_id))
}

/// Close the profile identified by `handle`. Returns [`CastellumStatus::Ok`] on
/// success or [`CastellumStatus::InvalidHandle`] if the handle is unknown.
#[no_mangle]
pub extern "C" fn castellum_close_profile(handle: u64) -> i32 {
    let status = catch_unwind(|| {
        clear_last_error();
        if handle == 0 {
            return CastellumStatus::InvalidHandle;
        }
        // Bound to its own statement so `registry_lock()`'s guard drops
        // before `stop_all_idle()` runs below — that call blocks joining
        // every account's IDLE thread, and if it ran inside the match
        // itself, the scrutinee temporary's extended lifetime would keep
        // the global profile registry locked for the whole join, stalling
        // every other in-flight FFI call (sync, idle start/stop/nudge, any
        // other command) on any other profile.
        let removed = registry_lock().remove(&handle);
        match removed {
            Some(profile) => {
                // Dropping this Arc closes the profile once the last
                // reference (e.g. an in-flight sync) is gone.
                profile.stop_all_idle();
                CastellumStatus::Ok
            }
            None => CastellumStatus::InvalidHandle,
        }
    })
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

// --- Trust surface (S48) -----------------------------------------------------

/// Analyse one link's (href, visible text) pair — real destination host,
/// text/href mismatch, punycode/IDN caution — as JSON in `*out_json`. Pure:
/// no profile handle, no I/O, never fails on malformed input (an empty or
/// unparseable href still produces a JSON object, never an error). Free the
/// result with `castellum_string_free`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_inspect_link(
    href: *const c_char,
    visible_text: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_json.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        let Some(href) = (unsafe { cstr(href) }) else {
            return CastellumStatus::InvalidArgument;
        };
        let visible_text = (unsafe { cstr(visible_text) }).unwrap_or_default();
        let inspection = castellum_mail::inspect_link(&href, &visible_text);
        match serde_json::to_string(&inspection) {
            Ok(json) => match CString::new(json) {
                Ok(c) => {
                    unsafe { *out_json = c.into_raw() };
                    CastellumStatus::Ok
                }
                Err(_) => CastellumStatus::Internal,
            },
            Err(_) => CastellumStatus::Internal,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Report what remote content a message's SANITIZED html tries to load
///: `{"remote_count","tracker_count","pixel_count",
/// "services":[{"name","count"}],"hosts":[{"host","count"}]}` in `*out_json`.
/// Pure: no profile handle, no I/O, never fetches anything, never fails on
/// malformed html. Free the result with `castellum_string_free`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_remote_content_report(
    sanitized_html: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let status = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        if out_json.is_null() {
            return CastellumStatus::InvalidArgument;
        }
        let Some(html) = (unsafe { cstr(sanitized_html) }) else {
            return CastellumStatus::InvalidArgument;
        };
        let report = castellum_mail::analyze_remote_content(&html);
        match serde_json::to_string(&report) {
            Ok(json) => match CString::new(json) {
                Ok(c) => {
                    unsafe { *out_json = c.into_raw() };
                    CastellumStatus::Ok
                }
                Err(_) => CastellumStatus::Internal,
            },
            Err(_) => CastellumStatus::Internal,
        }
    }))
    .unwrap_or(CastellumStatus::Internal);
    status.code()
}

/// Remember (or update) whether `from_address` may load remote content
/// automatically for `account_id` (S48). Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_set_sender_remote_content_allowed(
    handle: u64,
    account_id: *const c_char,
    from_address: *const c_char,
    allowed: i32,
) -> i32 {
    let (Some(account_id), Some(from_address)) =
        (unsafe { cstr(account_id) }, unsafe { cstr(from_address) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.set_sender_remote_content_allowed(&account_id, &from_address, allowed != 0)
    })
}

/// Write `*out_json` with `null` (no remembered policy) or `true`/`false`.
/// Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_sender_remote_content_allowed(
    handle: u64,
    account_id: *const c_char,
    from_address: *const c_char,
    out_json: *mut *mut c_char,
) -> i32 {
    let (Some(account_id), Some(from_address)) =
        (unsafe { cstr(account_id) }, unsafe { cstr(from_address) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_json(handle, out_json, |p| {
        p.sender_remote_content_allowed_json(&account_id, &from_address)
    })
}

/// Every remembered sender policy across every account, as a JSON array, for
/// the Settings review list. Returns a CastellumStatus code.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_list_sender_remote_content_policies(
    handle: u64,
    out_json: *mut *mut c_char,
) -> i32 {
    profile_json(handle, out_json, |p| {
        p.list_sender_remote_content_policies_json()
    })
}

/// Forget a remembered sender policy (Settings' Revoke action). Returns a
/// CastellumStatus code; a no-op (not an error) if none existed.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn castellum_revoke_sender_remote_content(
    handle: u64,
    account_id: *const c_char,
    from_address: *const c_char,
) -> i32 {
    let (Some(account_id), Some(from_address)) =
        (unsafe { cstr(account_id) }, unsafe { cstr(from_address) })
    else {
        return CastellumStatus::InvalidArgument.code();
    };
    profile_action(handle, |p| {
        p.revoke_sender_remote_content(&account_id, &from_address)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic while the profile registry lock is held must not brick every
    /// later FFI call. Before this, the registry's `Err(_) => Internal` arm
    /// made poisoning permanent: one panicking sync turned every subsequent
    /// call — on every profile — into `Internal` for the life of the process.
    #[test]
    fn a_poisoned_registry_does_not_brick_later_ffi_calls() {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-ffi-poison-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = CString::new(dir.join("profile.db").to_str().expect("utf8")).expect("cstring");

        let mut handle: u64 = 0;
        assert_eq!(
            castellum_open_profile(path.as_ptr(), &mut handle),
            CastellumStatus::Ok.code()
        );
        assert_ne!(handle, 0);

        let crashed = std::thread::spawn(|| {
            let _guard = registry_lock();
            panic!("simulated panic while holding the registry lock");
        })
        .join();
        assert!(crashed.is_err(), "the helper thread must have panicked");
        assert!(
            registry().is_poisoned(),
            "the registry mutex must actually be poisoned for this test to mean anything"
        );

        let mut version: i64 = -1;
        assert_eq!(
            castellum_profile_schema_version(handle, &mut version),
            CastellumStatus::Ok.code(),
            "the profile must still be reachable through a poisoned registry"
        );
        assert!(version >= 1, "real schema version, got {version}");

        assert_eq!(
            castellum_close_profile(handle),
            CastellumStatus::Ok.code(),
            "close must still work through a poisoned registry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_sync_replaces_a_stale_panic_payload_in_last_error() {
        // The panic hook records the RAW payload of EVERY panic — including
        // one `PanicGuardedProvider` catches and deliberately sanitises, whose
        // text embeds raw server bytes. `castellum_sync_account` must
        // therefore clear `last_error` on entry and record the core's own
        // redacted error on failure. When it only returned `e.status()`, the
        // stale raw payload was what Swift's `lastReason()` put in the sync
        // alert — and it stayed there for the next unrelated sync failure too.
        install_panic_hook();

        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-ffi-lasterr-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = CString::new(dir.join("profile.db").to_str().expect("utf8")).expect("cstring");
        let mut handle: u64 = 0;
        assert_eq!(
            castellum_open_profile(path.as_ptr(), &mut handle),
            CastellumStatus::Ok.code()
        );

        set_last_error(
            "internal panic at imap-proto/src/rfc3501.rs:379: server bytes 0x41 0x42".to_string(),
        );

        // Port 1 refuses immediately: a real connect failure, no network wait.
        let config = CString::new(
            r#"{"account_id":"acc1","email":"a@b.test","host":"127.0.0.1","port":1,
                "username":"u","password":"p","use_tls":false,"allow_invalid_certs":false}"#,
        )
        .expect("cstring");
        let mut out: *mut c_char = std::ptr::null_mut();
        let code = castellum_sync_account(handle, config.as_ptr(), &mut out);
        assert_ne!(code, CastellumStatus::Ok.code(), "the sync must fail");

        let recorded = LAST_ERROR
            .with(|e| e.borrow().clone())
            .expect("a failed sync must record a reason");
        assert!(
            !recorded.contains("0x41") && !recorded.contains("internal panic at"),
            "the stale raw panic payload must not survive into the UI: {recorded}"
        );

        assert_eq!(castellum_close_profile(handle), CastellumStatus::Ok.code());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caught_panic_is_recorded_to_last_error() {
        // The panic hook must turn an opaque caught panic into a legible
        // reason with the failing file:line, so the FFI's "internal error"
        // carries the actual cause.
        install_panic_hook();
        clear_last_error();
        let result = catch_unwind(AssertUnwindSafe(|| panic!("boom-sentinel")));
        assert!(result.is_err());
        let recorded = LAST_ERROR
            .with(|e| e.borrow().clone())
            .expect("panic hook set last_error");
        assert!(
            recorded.contains("internal panic at"),
            "expected location prefix, got: {recorded}"
        );
        assert!(
            recorded.contains("boom-sentinel"),
            "expected the panic message, got: {recorded}"
        );
    }
}
