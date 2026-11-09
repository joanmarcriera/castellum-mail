// Castellum - supervised per-account IMAP IDLE (S31).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// One background thread per account with an IDLE-capable server: connect,
// SELECT INBOX, IDLE, wait, and on activity/a 29-minute re-issue deadline/an
// explicit nudge, DONE + a targeted resync (record_and_run_sync, reusing the
// SAME already-connected provider) + IDLE again. An account whose server
// lacks IDLE gets no persistent loop at all — Swift's PeriodicSyncScheduler
// (unmodified cadence) is the poll fallback, so there is exactly one poll
// loop per account, never two (see this plan's Assumption 3).
//
// `next_action` below is a pure function with no I/O, mirroring
// PeriodicSyncScheduler.isDue's split: the thread body (this module's
// `run_account_idle`) is a thin driver that calls it, so the control-flow
// decisions are unit-testable without a socket or a real 29-minute wait.

use std::time::Duration;

/// What `idle_wait` (or a connect attempt) reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeSignal {
    Activity,
    Timeout,
    Error,
}

/// What arrived on the supervisor's control channel, if anything, since the
/// last decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlSignal {
    None,
    Nudge,
    Shutdown,
}

/// What the thread should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleAction {
    /// Nothing happened worth acting on — call `idle_wait` again.
    KeepIdling,
    /// DONE, run a targeted resync, then IDLE again.
    Resync,
    /// A real error (or the server hung up) — DONE/logout if possible and
    /// reconnect with backoff.
    Reconnect,
    /// DONE, logout, and exit the thread.
    Shutdown,
}

/// Decide the next action from one `idle_wait` tick's outcome, the control
/// channel, and how long the CURRENT idle command has been outstanding.
/// `Shutdown` always wins over everything else. An `Error` wake always means
/// `Reconnect` (RFC 2177 gives no partial-failure recovery within one IDLE).
/// Otherwise: real server activity, an explicit nudge, or having idled past
/// `reissue_after` all trigger `Resync` (which re-issues IDLE right after);
/// a bare timeout with none of those is `KeepIdling`.
pub(crate) fn next_action(
    wake: WakeSignal,
    control: ControlSignal,
    idle_elapsed: Duration,
    reissue_after: Duration,
) -> IdleAction {
    if control == ControlSignal::Shutdown {
        return IdleAction::Shutdown;
    }
    if wake == WakeSignal::Error {
        return IdleAction::Reconnect;
    }
    if wake == WakeSignal::Activity
        || control == ControlSignal::Nudge
        || idle_elapsed >= reissue_after
    {
        return IdleAction::Resync;
    }
    IdleAction::KeepIdling
}

/// Whether one `idle_wait` tick's outcome proves the connection healthy
/// enough to reset backoff to the floor (and report this account as
/// idle-active). Only a non-`Error` wake counts: `idle_start` completing
/// is NOT proof by itself — a server that accepts IDLE and then
/// immediately drops the connection must keep backing off, not retry at
/// `INITIAL_BACKOFF` forever (see the PARTIAL S31 review finding this
/// fixes: backoff used to reset right after `idle_start` succeeded).
pub(crate) fn wake_proves_health(wake: WakeSignal) -> bool {
    wake != WakeSignal::Error
}

use crate::imap_connect::connect_guarded_imap;
use crate::pass_gate;
use crate::redact::phase;
use crate::sync::{resync_on_live_connection, ImapSyncConfig};
use crate::sync_progress;
use crate::Profile;
use castellum_providers::{IdleWake, ImapConfig, MailProvider};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

/// How long one `idle_wait` tick blocks before the supervisor thread checks
/// its control channel again. This is also the worst-case latency of
/// `stop()` (Shutdown is only seen between ticks), so it stays at ~1 s: a
/// timed-out read is a cheap syscall, not a protocol round-trip.
const IDLE_WAIT_TICK: Duration = Duration::from_secs(1);
/// RFC 2177 recommends re-issuing IDLE well before a server's typical
/// 30-minute inactivity timeout; the roadmap specifies 29 minutes.
const IDLE_REISSUE_AFTER: Duration = Duration::from_secs(29 * 60);
#[cfg(not(test))]
const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Unit tests drive real reconnect cycles; a short floor keeps them fast
/// (backoff still doubles, which is all the growth tests look at).
#[cfg(test)]
const INITIAL_BACKOFF: Duration = Duration::from_millis(400);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// IDLE targets INBOX only (this plan's Assumption 4) — other mailboxes keep
/// relying on the periodic/manual sync path.
const IDLE_MAILBOX: &str = "INBOX";

/// How often a wake waiting for another pass of the same account to finish
/// re-checks its control channel (so Shutdown is never stuck behind a long
/// first sync).
const GATE_POLL: Duration = Duration::from_millis(200);

/// Whether an account's synced-folder selection includes INBOX, the only
/// mailbox IDLE watches (S31b, review finding e). `None` is the core's default
/// policy, which always syncs INBOX; an explicit list is what the user ticked
/// in Settings > Accounts > Folders… (`SyncScope.mailboxFilter`), and nothing
/// stops them unticking INBOX. IDLE must not run for such an account: its
/// resync would re-import INBOX into a store the user excluded it from.
pub(crate) fn idle_covers_inbox(mailboxes: Option<&[String]>) -> bool {
    mailboxes.is_none_or(|list| list.iter().any(|p| p.eq_ignore_ascii_case(IDLE_MAILBOX)))
}

pub(crate) enum IdleControl {
    Nudge,
    Shutdown,
}

struct AccountIdle {
    control: mpsc::Sender<IdleControl>,
    handle: thread::JoinHandle<()>,
    /// True only while the thread has a PROVEN, currently-idling connection
    /// (at least one non-`Error` `idle_wait` tick since the last connect).
    /// False while connecting, backed off after a failure, or once the
    /// server has been confirmed not to advertise IDLE at all — in every
    /// one of those states Swift's `PeriodicSyncScheduler` must see this
    /// account as NOT idle-active, so it keeps (or falls back to)
    /// `normalInterval` rather than the slower `idleSafetyNetInterval`
    /// meant for an account IDLE is actually covering.
    healthy: Arc<AtomicBool>,
}

/// Owns every account's supervised IDLE thread for one open `Profile`.
/// Interior-mutable (`Mutex`) so `Profile`'s own methods stay `&self`.
pub struct IdleSupervisor {
    accounts: Mutex<HashMap<String, AccountIdle>>,
    /// Serialises `start` (stop + spawn + insert) so two concurrent starts for
    /// one account cannot both find nothing to stop and orphan a thread. Kept
    /// apart from `accounts` because `stop` joins, which can take a while and
    /// must not block `nudge`/`is_running`.
    start_lock: Mutex<()>,
}

impl Default for IdleSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleSupervisor {
    pub(crate) fn new() -> Self {
        IdleSupervisor {
            accounts: Mutex::new(HashMap::new()),
            start_lock: Mutex::new(()),
        }
    }

    /// Start (or restart) `cfg.account_id`'s supervised IDLE thread.
    /// Idempotent-by-restart: an already-running thread for this account is
    /// stopped first (matches `castellum_idle_start`'s doc: a credential
    /// change or a restart-after-crash is just "start again").
    ///
    /// An account whose synced folders exclude INBOX gets NO thread (and any
    /// running one is stopped by the line below): `startIdle` with the new
    /// config after a folder edit is how a running IDLE is retired.
    pub(crate) fn start(&self, profile: Arc<Profile>, cfg: ImapSyncConfig) {
        let _serial = self.start_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.stop(&cfg.account_id);
        if !idle_covers_inbox(cfg.mailboxes.as_deref()) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let account_id = cfg.account_id.clone();
        let healthy = Arc::new(AtomicBool::new(false));
        let thread_healthy = Arc::clone(&healthy);
        let handle = thread::spawn(move || run_account_idle(profile, cfg, rx, thread_healthy));
        self.accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                account_id,
                AccountIdle {
                    control: tx,
                    handle,
                    healthy,
                },
            );
    }

    /// Stop `account_id`'s thread if running, joining it before returning —
    /// callers (including `Profile::stop_all_idle`, called from
    /// `castellum_close_profile`) can rely on the thread being fully gone
    /// once this returns, never merely signalled.
    pub(crate) fn stop(&self, account_id: &str) {
        let entry = self
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account_id);
        if let Some(entry) = entry {
            let _ = entry.control.send(IdleControl::Shutdown);
            let _ = entry.handle.join();
        }
    }

    /// Stop every account's thread. Called once from `castellum_close_profile`
    /// so a closed profile never leaves a background connection open.
    pub(crate) fn stop_all(&self) {
        let _serial = self.start_lock.lock().unwrap_or_else(|e| e.into_inner());
        let entries: Vec<AccountIdle> = self
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, v)| v)
            .collect();
        // Signal everyone first, then join: the threads wind down in parallel,
        // so closing N idle accounts waits one read tick, not N of them.
        for entry in &entries {
            let _ = entry.control.send(IdleControl::Shutdown);
        }
        for entry in entries {
            let _ = entry.handle.join();
        }
    }

    /// Wake `account_id`'s thread now (sleep/wake, network change). A no-op
    /// if that account has no running thread (e.g. IDLE-unsupported —
    /// nothing to nudge; Swift's periodic scheduler covers it).
    pub(crate) fn nudge(&self, account_id: &str) {
        if let Some(entry) = self
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account_id)
        {
            let _ = entry.control.send(IdleControl::Nudge);
        }
    }

    /// Whether `account_id` currently has a PROVEN, actively-idling
    /// connection — never merely "a thread object still exists in the map".
    /// An IDLE-incapable server, or one whose IDLE keeps failing, reports
    /// `false` here so the poll fallback (Swift's normal cadence) actually
    /// applies instead of the slower idle-safety-net cadence.
    pub(crate) fn is_running(&self, account_id: &str) -> bool {
        self.accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account_id)
            .is_some_and(|entry| entry.healthy.load(Ordering::Relaxed))
    }
}

/// One account's supervised loop. Connects; if the server doesn't advertise
/// IDLE, logs out and returns immediately (see this plan's Assumption 3 —
/// Swift's PeriodicSyncScheduler is the fallback, so this thread simply has
/// nothing to do). Otherwise idles INBOX, driving every tick through
/// `next_action`, until `IdleControl::Shutdown` or an unrecoverable error
/// after backoff has grown unreasonably (this loop never gives up
/// permanently on a recoverable server — only Shutdown ends it for good).
fn run_account_idle(
    profile: Arc<Profile>,
    cfg: ImapSyncConfig,
    control_rx: mpsc::Receiver<IdleControl>,
    healthy: Arc<AtomicBool>,
) {
    let imap_cfg = ImapConfig {
        host: cfg.host.clone(),
        port: cfg.port,
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        security: castellum_providers::TransportSecurity::resolve(
            cfg.imap_security.as_deref(),
            cfg.use_tls,
        ),
        allow_invalid_certs: cfg.allow_invalid_certs,
    };
    let inbox_filter = [IDLE_MAILBOX.to_string()];
    let mut backoff = INITIAL_BACKOFF;
    // This thread's resync passes must never publish sync progress: no Swift
    // attempt started them, and a live snapshot is what the app reads as "THIS
    // attempt got past login" (S31b, review finding b). See
    // `sync_progress::quiet_this_thread`.
    let _quiet = sync_progress::quiet_this_thread();
    // Set when a wake was seen but the connection died before its resync could
    // run (DONE failed): the next healthy connection owes that resync, so the
    // wake is never dropped (S31b, finding a).
    let mut owed_resync = false;
    // True once a connection has been proven idling. Any LATER (re)connect owes
    // one catch-up resync: the server pushes EXISTS only for changes after the
    // SELECT, so mail that landed while the old socket was dead is otherwise
    // invisible until the 29-minute re-issue or Swift's safety-net poll.
    let mut ever_idling = false;

    'connect: loop {
        // Never reported healthy while (re)connecting — only a PROVEN idle
        // tick below sets it, so a server stuck failing never reads as
        // idle-active even though this thread keeps retrying it.
        healthy.store(false, Ordering::Relaxed);
        match control_rx.try_recv() {
            Ok(IdleControl::Shutdown) | Err(TryRecvError::Disconnected) => return,
            _ => {}
        }

        let mut provider = match connect_guarded_imap(&imap_cfg) {
            Ok(p) => p,
            Err(_) => {
                if wait_out_backoff(&control_rx, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue 'connect;
            }
        };

        if !provider.idle_supported() {
            // No persistent loop for this account (this plan's Assumption
            // 3) — `healthy` is already false, so `is_running` reports
            // false immediately and Swift's periodic poll stays the
            // freshness mechanism at its normal cadence, never the slower
            // idle-safety-net one.
            let _ = provider.logout();
            return;
        }
        if provider.idle_start(IDLE_MAILBOX).is_err() {
            let _ = profile.db.record_account_sync_failure(
                &cfg.account_id,
                crate::redact::ErrorKind::Protocol.as_str(),
                "IMAP IDLE could not be started",
                phase::IDLE,
            );
            let _ = provider.logout();
            if wait_out_backoff(&control_rx, backoff) {
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue 'connect;
        }
        // `idle_start` succeeding means this account IS actively idling —
        // report it right away (existing tests rely on this being visible
        // within milliseconds, not after a full `IDLE_WAIT_TICK`). This is
        // deliberately NOT when backoff resets — see below.
        healthy.store(true, Ordering::Relaxed);
        if ever_idling {
            owed_resync = true;
        }
        ever_idling = true;
        let mut idle_started_at = Instant::now();

        loop {
            // `from_server` is false for the synthetic wake that replays an owed
            // resync: nothing came back from the wire, so it proves nothing.
            let (wake, from_server) = if std::mem::take(&mut owed_resync) {
                (WakeSignal::Activity, false)
            } else {
                let wake = match provider.idle_wait(IDLE_WAIT_TICK) {
                    Ok(IdleWake::Activity) => WakeSignal::Activity,
                    Ok(IdleWake::Timeout) => WakeSignal::Timeout,
                    Err(_) => WakeSignal::Error,
                };
                (wake, true)
            };
            // Backoff resets to the floor only once a TICK has come back
            // without an error — `idle_start` succeeding is not proof by
            // itself (a server that accepts IDLE and then immediately drops
            // the connection must keep backing off, never retry at the
            // floor forever — the PARTIAL S31 review finding this fixes).
            // The synthetic owed-resync wake must not reset it: a server whose
            // DONE keeps failing would otherwise be retried at the floor forever.
            if from_server && wake_proves_health(wake) {
                backoff = INITIAL_BACKOFF;
            } else if wake == WakeSignal::Error {
                healthy.store(false, Ordering::Relaxed);
            }
            let control = match control_rx.try_recv() {
                Ok(IdleControl::Nudge) => ControlSignal::Nudge,
                Ok(IdleControl::Shutdown) => ControlSignal::Shutdown,
                Err(TryRecvError::Empty) => ControlSignal::None,
                // The supervisor dropped us (orphaned by a replaced entry):
                // treat it as Shutdown so no unreachable thread lingers.
                Err(TryRecvError::Disconnected) => ControlSignal::Shutdown,
            };

            match next_action(wake, control, idle_started_at.elapsed(), IDLE_REISSUE_AFTER) {
                IdleAction::KeepIdling => continue,
                IdleAction::Shutdown => {
                    let _ = provider.idle_done();
                    let _ = provider.logout();
                    return;
                }
                IdleAction::Reconnect => {
                    let _ = provider.logout();
                    if wait_out_backoff(&control_rx, backoff) {
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue 'connect;
                }
                IdleAction::Resync => {
                    // Another pass for this account (a periodic, manual or
                    // launch sync driven from Swift) may be running; passes
                    // are mutually exclusive in the core (`pass_gate`), so
                    // wait for it here, in interruptible slices, and run
                    // this wake as the follow-up pass. Waiting BEFORE DONE
                    // keeps IDLE armed meanwhile.
                    let ready = pass_gate::wait_until_idle(
                        pass_gate::scope_of(&profile.db),
                        &cfg.account_id,
                        GATE_POLL,
                        || {
                            matches!(
                                control_rx.try_recv(),
                                Ok(IdleControl::Shutdown) | Err(TryRecvError::Disconnected)
                            )
                        },
                    );
                    if !ready {
                        let _ = provider.idle_done();
                        let _ = provider.logout();
                        return;
                    }
                    if provider.idle_done().is_err() {
                        owed_resync = true;
                        healthy.store(false, Ordering::Relaxed);
                        let _ = provider.logout();
                        if wait_out_backoff(&control_rx, backoff) {
                            return;
                        }
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue 'connect;
                    }
                    // resync_on_live_connection never logs out — the same
                    // provider keeps its connection for the IDLE re-issue
                    // right below. It also unlinks the cached attachment
                    // files of a folder this pass removed, which is why the
                    // report itself can be dropped here.
                    let pass = resync_on_live_connection(
                        profile.path(),
                        &profile.db,
                        &cfg.account_id,
                        &cfg.email,
                        Some(&inbox_filter[..]),
                        &mut provider,
                        &cfg.host,
                    );
                    // The pass may have rewritten rows with no arrival; tell
                    // the app's poller so the open list reloads (#3040).
                    profile.idle_pass_epoch.fetch_add(1, Ordering::Relaxed);
                    // A per-mailbox failure (SELECT/fetch on INBOX) comes back
                    // as Ok(report) with `error` set; it is a failed pass too.
                    let pass_failed = match &pass {
                        Err(_) => true,
                        Ok(report) => report.mailboxes.iter().any(|m| m.error.is_some()),
                    };
                    if pass_failed || provider.session_abandoned() {
                        // The wake produced no further notification, so a
                        // failed or abandoned pass must be retried on the
                        // next connection, never dropped.
                        owed_resync = true;
                        healthy.store(false, Ordering::Relaxed);
                        if pass_failed {
                            let _ = provider.logout();
                        }
                        if wait_out_backoff(&control_rx, backoff) {
                            return;
                        }
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue 'connect;
                    }
                    if provider.idle_start(IDLE_MAILBOX).is_err() {
                        healthy.store(false, Ordering::Relaxed);
                        let _ = profile.db.record_account_sync_failure(
                            &cfg.account_id,
                            crate::redact::ErrorKind::Protocol.as_str(),
                            "IMAP IDLE could not be re-issued after a resync",
                            phase::IDLE,
                        );
                        let _ = provider.logout();
                        if wait_out_backoff(&control_rx, backoff) {
                            return;
                        }
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue 'connect;
                    }
                    healthy.store(true, Ordering::Relaxed);
                    idle_started_at = Instant::now();
                }
            }
        }
    }
}

/// Sleep for `backoff`, but wake early on Shutdown. Returns `true` if the
/// caller should exit the thread entirely (Shutdown or a disconnected
/// channel), `false` if it should retry the connect loop.
fn wait_out_backoff(control_rx: &mpsc::Receiver<IdleControl>, backoff: Duration) -> bool {
    match control_rx.recv_timeout(backoff) {
        Ok(IdleControl::Shutdown) | Err(RecvTimeoutError::Disconnected) => true,
        Ok(IdleControl::Nudge) | Err(RecvTimeoutError::Timeout) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_always_wins() {
        assert_eq!(
            next_action(
                WakeSignal::Activity,
                ControlSignal::Shutdown,
                Duration::ZERO,
                Duration::from_secs(60)
            ),
            IdleAction::Shutdown
        );
        assert_eq!(
            next_action(
                WakeSignal::Error,
                ControlSignal::Shutdown,
                Duration::ZERO,
                Duration::from_secs(60)
            ),
            IdleAction::Shutdown
        );
    }

    #[test]
    fn a_wake_error_reconnects_even_without_shutdown() {
        assert_eq!(
            next_action(
                WakeSignal::Error,
                ControlSignal::None,
                Duration::ZERO,
                Duration::from_secs(60)
            ),
            IdleAction::Reconnect
        );
    }

    #[test]
    fn activity_resyncs() {
        assert_eq!(
            next_action(
                WakeSignal::Activity,
                ControlSignal::None,
                Duration::ZERO,
                Duration::from_secs(60)
            ),
            IdleAction::Resync
        );
    }

    #[test]
    fn a_nudge_resyncs_even_on_a_bare_timeout() {
        assert_eq!(
            next_action(
                WakeSignal::Timeout,
                ControlSignal::Nudge,
                Duration::from_secs(1),
                Duration::from_secs(60)
            ),
            IdleAction::Resync
        );
    }

    #[test]
    fn idling_past_the_reissue_deadline_resyncs() {
        assert_eq!(
            next_action(
                WakeSignal::Timeout,
                ControlSignal::None,
                Duration::from_secs(61),
                Duration::from_secs(60)
            ),
            IdleAction::Resync
        );
    }

    #[test]
    fn a_bare_timeout_under_the_deadline_keeps_idling() {
        assert_eq!(
            next_action(
                WakeSignal::Timeout,
                ControlSignal::None,
                Duration::from_secs(5),
                Duration::from_secs(60)
            ),
            IdleAction::KeepIdling
        );
    }

    #[test]
    fn exactly_at_the_reissue_deadline_resyncs() {
        // >= , not >, so a slow-ticking thread can never overshoot 29
        // minutes without at least attempting a re-issue.
        assert_eq!(
            next_action(
                WakeSignal::Timeout,
                ControlSignal::None,
                Duration::from_secs(60),
                Duration::from_secs(60)
            ),
            IdleAction::Resync
        );
    }

    #[test]
    fn only_a_non_error_wake_proves_the_connection_healthy() {
        assert!(wake_proves_health(WakeSignal::Activity));
        assert!(wake_proves_health(WakeSignal::Timeout));
        assert!(!wake_proves_health(WakeSignal::Error));
    }

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    struct Server {
        port: u16,
        handle: Option<thread::JoinHandle<()>>,
        /// Set the instant `accept()` returns a connection — the only
        /// reliable "the client actually reached the wire" signal now that
        /// `is_running` goes false immediately on `IdleSupervisor::start`
        /// (before the thread connects), not only once a server is proven
        /// unsupported. Without this, a test that stops the supervisor as
        /// soon as `is_running` reads false can race the spawned thread's
        /// very first `try_recv` and win, so the thread never connects at
        /// all — leaving this `Server`'s own `accept()` blocked forever and
        /// `Drop`'s `join()` hanging with it. `accept()` itself is now
        /// bounded (below) as defence in depth: even a genuine test
        /// failure (the client never connects at all, for any reason) must
        /// make THIS thread give up and return, never hang the whole
        /// suite — `Drop`'s `join()` can then still fail the test loudly
        /// instead of freezing it.
        connected: Arc<AtomicBool>,
    }

    /// Bound on how long `Server`'s `accept()` waits for a connection
    /// before giving up and returning — see `connected`'s doc comment.
    /// Generous: well over any realistic connect + one backoff cycle, even
    /// under heavy shared-machine contention.
    const SERVER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(90);

    impl Server {
        /// `idle_capable` controls the CAPABILITY reply; when true, IDLE
        /// round-trips forever (continuation, then block until DONE) so the
        /// test drives shutdown/nudge itself rather than racing a timeout.
        fn start(idle_capable: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let connected = Arc::new(AtomicBool::new(false));
            let thread_connected = Arc::clone(&connected);
            let handle = thread::spawn(move || {
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = Instant::now() + SERVER_ACCEPT_TIMEOUT;
                let stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => return,
                    }
                };
                // Accepted sockets are not guaranteed to inherit the
                // listener's non-blocking flag across platforms — force
                // blocking I/O for the rest of this scripted exchange.
                stream.set_nonblocking(false).expect("blocking");
                thread_connected.store(true, Ordering::Relaxed);
                let mut writer = stream.try_clone().expect("clone");
                let mut reader = BufReader::new(stream);
                let _ = writer.write_all(b"* OK test ready\r\n");
                let _ = writer.flush();
                let caps = if idle_capable {
                    "* CAPABILITY IMAP4rev1 IDLE\r\n"
                } else {
                    "* CAPABILITY IMAP4rev1\r\n"
                };
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    let mut parts = trimmed.splitn(2, ' ');
                    let tag = parts.next().unwrap_or("*").to_string();
                    let command = parts.next().unwrap_or("").to_ascii_uppercase();
                    if command.starts_with("LOGOUT") {
                        let _ = writer.write_all(b"* BYE\r\n");
                        let _ =
                            writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
                        let _ = writer.flush();
                        return;
                    }
                    if command.starts_with("LOGIN") {
                        let _ =
                            writer.write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes());
                    } else if command.starts_with("CAPABILITY") {
                        let _ = writer.write_all(caps.as_bytes());
                        let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
                    } else if command.starts_with("SELECT") {
                        let _ = writer.write_all(b"* 0 EXISTS\r\n");
                        let _ = writer.write_all(
                            format!("{tag} OK [READ-WRITE] SELECT completed\r\n").as_bytes(),
                        );
                    } else if command.starts_with("IDLE") {
                        let _ = writer.write_all(b"+ idling\r\n");
                        let _ = writer.flush();
                        line.clear();
                        let _ = reader.read_line(&mut line); // "DONE"
                        let _ = writer.write_all(format!("{tag} OK IDLE completed\r\n").as_bytes());
                    } else {
                        let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
                    }
                    let _ = writer.flush();
                }
            });
            Server {
                port,
                handle: Some(handle),
                connected,
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn test_profile() -> Arc<Profile> {
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-idle-test-{}-{}",
            uuid_like(),
            TEST_PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        Arc::new(crate::open_profile(&dir).expect("open profile"))
    }

    /// Guarantees `test_profile()`'s directory is unique on its own — a
    /// nanosecond timestamp ALONE collided under enough concurrent callers
    /// (this module's tests run multi-threaded by default): two threads
    /// reading `SystemTime::now()` in the same tick raced to `open_profile`
    /// the SAME directory, and one saw `Io`. Same pattern as `sync.rs`'s
    /// `temp_db`.
    static TEST_PROFILE_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    fn uuid_like() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    fn cfg_for(port: u16) -> ImapSyncConfig {
        serde_json::from_str(&format!(
            r#"{{"account_id":"acct","email":"a@example.com","host":"127.0.0.1","port":{port},"username":"pilot","password":"app-password","use_tls":false}}"#
        ))
        .expect("valid config json")
    }

    #[test]
    fn an_idle_incapable_server_never_starts_a_persistent_loop() {
        let server = Server::start(false);
        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(server.port));
        // Wait for the thread to actually reach the wire first (bounded
        // poll, not a fixed sleep guess) — `is_running` now goes false the
        // instant `start` returns (before the thread has connected at all),
        // so checking it BEFORE the connection happens would race the
        // thread's very first control-channel check and could stop it
        // before it ever touches this `Server`, leaving `accept()` (and
        // this test's own `Drop for Server` join) blocked forever (now
        // bounded at `SERVER_ACCEPT_TIMEOUT` regardless, as defence in
        // depth). Generous margin, comfortably under `SERVER_ACCEPT_TIMEOUT`
        // — observed empirically to need >30s under heavy multi-agent
        // shared-machine CPU contention (thread scheduling starvation, not
        // a logic failure), not just one connect + one backoff cycle.
        let deadline = Instant::now() + Duration::from_secs(60);
        while !server.connected.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline, "the thread never connected");
            thread::sleep(Duration::from_millis(20));
        }
        // The thread has now connected, seen no IDLE, logged out and exits
        // on its own — poll (bounded) rather than sleep a fixed guess.
        let mut became_not_running = false;
        for _ in 0..50 {
            if !supervisor.is_running("acct") {
                became_not_running = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // REQUIRED BY THE S31 REVIEW (2026-09-25): the old version of this
        // test polled `is_running` in a loop that could fall through
        // WITHOUT ever asserting the break happened — a stuck-true
        // `is_running` (the "never removes its own map entry" bug) made
        // this test pass either way. Assert the break actually fired.
        assert!(
            became_not_running,
            "an IDLE-incapable account must stop reporting is_running (poll fallback), \
             not stay true for the full 1s poll budget"
        );
        // stop() is still safe to call even if the thread already exited on
        // its own (the map entry may or may not still be present).
        supervisor.stop("acct");
    }

    #[test]
    fn stop_joins_a_running_idle_capable_thread() {
        let server = Server::start(true);
        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(server.port));
        // Wait (bounded) for the thread to reach the idle_wait loop: `is_running`
        // turns true only after connect + login + SELECT + IDLE, which a fixed
        // 100 ms sleep did not cover on a loaded CI runner (2026-10-17, load ~60).
        wait_for("idle", || supervisor.is_running("acct"));
        supervisor.stop("acct");
        assert!(!supervisor.is_running("acct"));
    }

    #[test]
    fn profile_stop_all_idle_joins_every_account() {
        let server = Server::start(true);
        let profile = test_profile();
        profile
            .idle
            .start(Arc::clone(&profile), cfg_for(server.port));
        thread::sleep(Duration::from_millis(100));
        assert!(profile.is_idle_running("acct"));
        profile.stop_all_idle();
        assert!(!profile.is_idle_running("acct"));
    }

    // REQUIRED BY THE S31 REVIEW (2026-09-25): a server that accepts IDLE and
    // then immediately drops the connection used to make backoff reset to
    // `INITIAL_BACKOFF` on every reconnect (it reset right after `idle_start`
    // succeeded, not after a PROVEN idle tick) — retried every 5s forever
    // instead of backing off. This drives the real `run_account_idle` thread
    // against a server that accepts IDLE, sends the `+` continuation, then
    // hangs up before ever answering DONE, and proves from the wall-clock
    // gap between successive reconnects that backoff actually grows, and
    // that `is_running` reads false while it is failing (a real poll
    // fallback, not the 15-minute idle-safety-net cadence).
    #[test]
    fn backoff_grows_when_idle_keeps_accepting_and_then_dropping_the_connection() {
        let accepts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server_accepts = Arc::clone(&accepts);
        let server_handle = thread::spawn(move || {
            // Three reconnects are enough to see two backoff gaps; stop
            // accepting after that so a stray 4th attempt just fails fast.
            for _ in 0..3 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                server_accepts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Instant::now());
                let mut writer = stream.try_clone().expect("clone");
                let mut reader = BufReader::new(stream);
                let _ = writer.write_all(b"* OK test ready\r\n");
                let _ = writer.flush();
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    let mut parts = trimmed.splitn(2, ' ');
                    let tag = parts.next().unwrap_or("*").to_string();
                    let command = parts.next().unwrap_or("").to_ascii_uppercase();
                    if command.starts_with("LOGIN") {
                        let _ =
                            writer.write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes());
                    } else if command.starts_with("CAPABILITY") {
                        let _ = writer.write_all(b"* CAPABILITY IMAP4rev1 IDLE\r\n");
                        let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
                    } else if command.starts_with("SELECT") {
                        let _ = writer.write_all(b"* 0 EXISTS\r\n");
                        let _ = writer.write_all(
                            format!("{tag} OK [READ-WRITE] SELECT completed\r\n").as_bytes(),
                        );
                    } else if command.starts_with("IDLE") {
                        let _ = writer.write_all(b"+ idling\r\n");
                        let _ = writer.flush();
                        // Hang up without ever answering DONE — the client's
                        // next `idle_wait` read must see this as an error,
                        // not a timeout.
                        break;
                    } else {
                        let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
                    }
                    let _ = writer.flush();
                }
                // Drop both handles now, explicitly, before the next accept.
            }
        });

        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(port));

        // Wait for all three accepts (generous bound: two backoff waits at
        // up to MAX_BACKOFF each, though in practice ~15s at the floor).
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if accepts.lock().unwrap_or_else(|e| e.into_inner()).len() >= 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "must reconnect three times within the bound"
            );
            thread::sleep(Duration::from_millis(50));
        }

        supervisor.stop("acct");
        assert!(!supervisor.is_running("acct"));
        let _ = server_handle.join();

        let ts = accepts.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let gap1 = ts[1].duration_since(ts[0]);
        let gap2 = ts[2].duration_since(ts[1]);
        assert!(
            gap2 > gap1.mul_f64(1.5),
            "backoff must grow across reconnects, not reset to the floor \
             after every successful idle_start (gap1={gap1:?}, gap2={gap2:?})"
        );
    }

    // An owed resync (DONE failed before its resync could run) is replayed as
    // a synthetic wake on the next connection. That wake never came off the
    // wire, so it must NOT reset backoff to the floor: a server whose DONE
    // keeps failing was retried every 5s forever. Here the server announces
    // activity during IDLE (so the first connection owes a resync) and hangs
    // up on DONE every time; the second gap must still be bigger than the first.
    #[test]
    fn backoff_grows_when_done_keeps_failing_with_a_resync_owed() {
        let accepts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server_accepts = Arc::clone(&accepts);
        let server_handle = thread::spawn(move || {
            for _ in 0..3 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                server_accepts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Instant::now());
                let mut writer = stream.try_clone().expect("clone");
                let mut reader = BufReader::new(stream);
                let _ = writer.write_all(b"* OK test ready\r\n");
                let _ = writer.flush();
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    let mut parts = trimmed.splitn(2, ' ');
                    let tag = parts.next().unwrap_or("*").to_string();
                    let command = parts.next().unwrap_or("").to_ascii_uppercase();
                    if command.starts_with("LOGIN") {
                        let _ =
                            writer.write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes());
                    } else if command.starts_with("CAPABILITY") {
                        let _ = writer.write_all(b"* CAPABILITY IMAP4rev1 IDLE\r\n");
                        let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
                    } else if command.starts_with("SELECT") {
                        let _ = writer.write_all(b"* 0 EXISTS\r\n");
                        let _ = writer.write_all(
                            format!("{tag} OK [READ-WRITE] SELECT completed\r\n").as_bytes(),
                        );
                    } else if command.starts_with("IDLE") {
                        // Continuation plus untagged activity, then wait for
                        // DONE and hang up without ever answering it.
                        let _ = writer.write_all(b"+ idling\r\n* 1 EXISTS\r\n");
                        let _ = writer.flush();
                        line.clear();
                        let _ = reader.read_line(&mut line);
                        break;
                    } else {
                        let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
                    }
                    let _ = writer.flush();
                }
            }
        });

        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(port));

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if accepts.lock().unwrap_or_else(|e| e.into_inner()).len() >= 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "must reconnect three times within the bound"
            );
            thread::sleep(Duration::from_millis(50));
        }

        supervisor.stop("acct");
        let _ = server_handle.join();

        let ts = accepts.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let gap1 = ts[1].duration_since(ts[0]);
        let gap2 = ts[2].duration_since(ts[1]);
        assert!(
            gap2 > gap1.mul_f64(1.5),
            "an owed-resync wake must not reset backoff to the floor \
             (gap1={gap1:?}, gap2={gap2:?})"
        );
    }

    use castellum_models::{Account, Address, Mailbox, NewMessage, SyncCursor};

    fn seed_account_mailbox(db: &castellum_db::Database) {
        db.upsert_account(&Account {
            id: "acct".into(),
            email: "a@example.com".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled("acct", true).unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acct:INBOX".into(),
            account_id: "acct".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
    }

    fn seed_inbox_cursor(
        db: &castellum_db::Database,
        uid_validity: i64,
        uid_next: i64,
        modseq: i64,
    ) {
        db.set_sync_cursor(
            "acct",
            "acct:INBOX",
            &SyncCursor {
                uid_validity: Some(uid_validity),
                uid_next: Some(uid_next),
                highest_modseq: Some(modseq),
                descriptor_checkpoint_uid: None,
            },
        )
        .unwrap();
    }

    fn seed_message(db: &castellum_db::Database, uid: i64) {
        db.upsert_message(&NewMessage {
            id: format!("acct:INBOX:{uid}"),
            account_id: "acct".into(),
            mailbox_id: "acct:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<m{uid}@example.com>")),
            subject: Some("test".into()),
            from_name: Some("Alice".into()),
            from_address: Some("alice@example.com".into()),
            sent_at: Some(1_788_000_000 + uid),
            snippet: None,
            size_bytes: Some(1000),
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![Address {
                kind: "from".into(),
                name: Some("Alice".into()),
                address: "alice@example.com".into(),
            }],
            text_body: None,
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    // REQUIRED BY THE S30 REVIEW (2026-09-25), closed here: a `* VANISHED`
    // received DURING IDLE with QRESYNC enabled must wake the incremental
    // sync AND that sync must actually apply the expunge locally — not
    // merely "wake" in the abstract. This drives the REAL `run_account_idle`
    // thread (via `IdleSupervisor`, exactly as production does) against a
    // scripted server that: negotiates QRESYNC, lets `idle_start`'s plain
    // SELECT through, pushes `* VANISHED 7` mid-IDLE, then — on the
    // `record_and_run_sync` resync's OWN `SELECT ... QRESYNC` — replies with
    // the same VANISHED as a QRESYNC delta before hanging up (so the
    // re-issued IDLE fails fast instead of idling out the 20s tick). Seeds
    // uid 7 into the DB first, and asserts it is gone afterwards.
    #[test]
    fn a_vanished_wake_mid_idle_causes_the_incremental_resync_to_remove_the_message() {
        vanished_wake_scenario();
    }

    // S31b review, finding b: an IDLE-woken resync must not publish a live
    // sync-progress snapshot. The app reads a live snapshot as proof that its
    // own attempt got past login (the early credential save), so an IDLE pass
    // overlapping a Reconnect with a wrong password would get that password
    // saved. Same real thread and scripted server as above, which really does
    // run a resync pass.
    #[test]
    fn an_idle_resync_leaves_no_sync_progress_snapshot_behind() {
        let _profile = vanished_wake_scenario();
        assert!(
            sync_progress::snapshot("acct").is_none(),
            "the IDLE pass published progress: {:?}",
            sync_progress::snapshot("acct")
        );
    }

    /// Drives the real `run_account_idle` thread through one VANISHED wake and
    /// its resync (see the comment above the first test using it); returns the
    /// profile after the thread was stopped.
    fn vanished_wake_scenario() -> Arc<Profile> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server_handle = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let _ = writer.write_all(b"* OK test ready\r\n");
            let _ = writer.flush();
            let mut selects = 0u32;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let trimmed = line.trim_end_matches(['\r', '\n']);
                let mut parts = trimmed.splitn(2, ' ');
                let tag = parts.next().unwrap_or("*").to_string();
                let command = parts.next().unwrap_or("").to_ascii_uppercase();
                if command.starts_with("LOGIN") {
                    let _ = writer.write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes());
                } else if command.starts_with("CAPABILITY") {
                    let _ = writer
                        .write_all(b"* CAPABILITY IMAP4rev1 IDLE CONDSTORE QRESYNC ENABLE\r\n");
                    let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
                } else if command.starts_with("ENABLE") {
                    let _ = writer.write_all(b"* ENABLED CONDSTORE QRESYNC\r\n");
                    let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
                } else if command.starts_with("LIST") {
                    // record_and_run_sync's mailbox loop needs INBOX to
                    // exist via list_mailboxes() before it ever calls
                    // select/select_qresync on it.
                    let _ = writer.write_all(b"* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n");
                    let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
                } else if command.starts_with("SELECT") {
                    selects += 1;
                    if selects == 1 {
                        // idle_start's plain SELECT.
                        let _ = writer.write_all(b"* 1 EXISTS\r\n");
                        let _ = writer.write_all(
                            format!("{tag} OK [READ-WRITE] SELECT completed\r\n").as_bytes(),
                        );
                        let _ = writer.flush();
                    } else {
                        // record_and_run_sync's own select_qresync, replying
                        // with the same expunge as a QRESYNC delta.
                        let _ = writer.write_all(b"* 0 EXISTS\r\n");
                        let _ = writer.write_all(b"* OK [UIDVALIDITY 9] UIDs valid\r\n");
                        let _ = writer.write_all(b"* OK [UIDNEXT 100] Predicted next UID\r\n");
                        let _ = writer.write_all(b"* OK [HIGHESTMODSEQ 600] Highest\r\n");
                        let _ = writer.write_all(b"* VANISHED (EARLIER) 7\r\n");
                        let _ = writer.write_all(
                            format!("{tag} OK [READ-WRITE] SELECT completed\r\n").as_bytes(),
                        );
                        let _ = writer.flush();
                        // Hang up now: the re-issued idle_start's SELECT
                        // must fail fast rather than idle out a 20s tick.
                        return;
                    }
                } else if command.starts_with("IDLE") {
                    let _ = writer.write_all(b"+ idling\r\n");
                    let _ = writer.write_all(b"* VANISHED 7\r\n");
                    let _ = writer.flush();
                    line.clear();
                    let _ = reader.read_line(&mut line); // "DONE"
                    let _ = writer.write_all(format!("{tag} OK IDLE completed\r\n").as_bytes());
                } else if command.starts_with("LOGOUT") {
                    let _ = writer.write_all(b"* BYE\r\n");
                    let _ = writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
                    let _ = writer.flush();
                    return;
                } else {
                    let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
                }
                let _ = writer.flush();
            }
        });

        let profile = test_profile();
        seed_account_mailbox(&profile.db);
        seed_inbox_cursor(&profile.db, 9, 100, 500);
        seed_message(&profile.db, 7);
        assert!(
            profile
                .db
                .get_message_detail("acct:INBOX:7")
                .unwrap()
                .is_some(),
            "the message must exist before the wake"
        );

        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(port));

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if profile
                .db
                .get_message_detail("acct:INBOX:7")
                .unwrap()
                .is_none()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the VANISHED-triggered resync must remove the message within the bound"
            );
            thread::sleep(Duration::from_millis(50));
        }

        supervisor.stop("acct");
        let _ = server_handle.join();
        assert!(
            profile.idle_pass_epoch() >= 1,
            "a finished IDLE resync pass must bump the epoch the app polls (#3040)"
        );
        profile
    }

    // S31b review, finding e: INBOX can be unticked in Settings > Accounts >
    // Folders… (`FolderChecklist` toggles every selectable folder; `canSave`
    // only needs a non-empty selection), so IDLE must follow the account's
    // synced folders.
    #[test]
    fn idle_follows_the_synced_folders_and_never_runs_without_inbox() {
        let inbox = |l: &[&str]| -> Vec<String> { l.iter().map(|s| s.to_string()).collect() };
        assert!(idle_covers_inbox(None), "default policy always syncs INBOX");
        assert!(idle_covers_inbox(Some(&inbox(&["Sent", "inbox"]))));
        assert!(!idle_covers_inbox(Some(&inbox(&["Sent", "Archive"]))));
        assert!(!idle_covers_inbox(Some(&[])));

        // Running with INBOX selected...
        let server = Server::start(true);
        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(server.port));
        let deadline = Instant::now() + Duration::from_secs(60);
        while !server.connected.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline, "the thread never connected");
            thread::sleep(Duration::from_millis(20));
        }
        wait_for("idle", || supervisor.is_running("acct"));

        // ...then the user unticks INBOX and Swift restarts IDLE with the new
        // config: the running thread must be stopped and none started.
        let mut excluded = cfg_for(1);
        excluded.mailboxes = Some(inbox(&["Sent", "Archive"]));
        supervisor.start(Arc::clone(&profile), excluded);
        assert!(!supervisor.is_running("acct"));
        assert!(
            !supervisor
                .accounts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("acct"),
            "no IDLE thread may exist for an account whose folders exclude INBOX"
        );
    }

    /// Commands the scripted catch-up servers treat as "just the IDLE
    /// handshake"; anything else after DONE is a sync pass.
    const HANDSHAKE: [&str; 6] = ["LOGIN", "CAPABILITY", "SELECT", "IDLE", "DONE", "LOGOUT"];

    /// What a scripted connection does when the client sends IDLE.
    #[derive(Clone, Copy)]
    enum IdleScript {
        /// `+` continuation, then hang up (socket dies mid-IDLE).
        HangUp,
        /// `+` continuation plus `* 1 EXISTS`, answer DONE, then reject
        /// every later command with BAD (the resync fails).
        ActivityThenReject,
        /// `+` continuation, answer DONE, accept everything after.
        Quiet,
        /// Like `ActivityThenReject` (one `* 1 EXISTS`) but the failure is
        /// per-mailbox: LIST names INBOX and the first SELECT after DONE
        /// answers NO, so the pass returns Ok with a mailbox error.
        ExistsThenSelectNo,
        /// No EXISTS; the first SELECT after DONE (the owed catch-up pass)
        /// answers NO. Everything else is accepted.
        QuietSelectNo,
    }

    type CmdLog = Arc<Mutex<Vec<(usize, String)>>>;

    /// Accepts `scripts.len()` connections in order, logs `(connection,
    /// command word)` for every client line, and plays each connection's
    /// `IdleScript`.
    fn scripted_server(scripts: Vec<IdleScript>) -> (u16, CmdLog, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log: CmdLog = Arc::new(Mutex::new(Vec::new()));
        let server_log = Arc::clone(&log);
        let started = Instant::now();
        let handle = thread::spawn(move || {
            for (n, script) in scripts.into_iter().enumerate() {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut writer = stream.try_clone().expect("clone");
                let mut reader = BufReader::new(stream);
                let _ = writer.write_all(b"* OK test ready\r\n");
                let mut rejecting = false;
                let mut after_done = false;
                let mut select_failed = false;
                // `@ms` entries timestamp each connection (ignored by
                // `sync_ran_on`) so tests can compare reconnect gaps.
                server_log
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((n, format!("@{}", started.elapsed().as_millis())));
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    let mut parts = trimmed.splitn(2, ' ');
                    let tag = parts.next().unwrap_or("*").to_string();
                    let command = parts.next().unwrap_or("").to_ascii_uppercase();
                    let word = command.split(' ').next().unwrap_or("").to_string();
                    server_log
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((n, word.clone()));
                    if word == "LOGOUT" {
                        let _ = writer.write_all(b"* BYE\r\n");
                        let _ =
                            writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
                        let _ = writer.flush();
                        break;
                    }
                    if rejecting {
                        let _ = writer.write_all(format!("{tag} BAD rejected\r\n").as_bytes());
                    } else if word == "LOGIN" {
                        let _ =
                            writer.write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes());
                    } else if word == "CAPABILITY" {
                        let _ = writer.write_all(b"* CAPABILITY IMAP4rev1 IDLE\r\n");
                        let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
                    } else if word == "LIST"
                        && matches!(
                            script,
                            IdleScript::ExistsThenSelectNo | IdleScript::QuietSelectNo
                        )
                    {
                        let _ = writer.write_all(b"* LIST () \"/\" \"INBOX\"\r\n");
                        let _ = writer.write_all(format!("{tag} OK LIST completed\r\n").as_bytes());
                    } else if word == "SELECT" && after_done && !select_failed {
                        select_failed = true;
                        let _ = writer.write_all(format!("{tag} NO mailbox busy\r\n").as_bytes());
                    } else if word == "SELECT" {
                        let _ = writer.write_all(b"* 0 EXISTS\r\n");
                        let _ = writer.write_all(
                            format!("{tag} OK [READ-WRITE] SELECT completed\r\n").as_bytes(),
                        );
                    } else if word == "IDLE" {
                        let _ = match script {
                            IdleScript::ActivityThenReject | IdleScript::ExistsThenSelectNo => {
                                writer.write_all(b"+ idling\r\n* 1 EXISTS\r\n")
                            }
                            _ => writer.write_all(b"+ idling\r\n"),
                        };
                        let _ = writer.flush();
                        if matches!(script, IdleScript::HangUp) {
                            break;
                        }
                        line.clear();
                        let _ = reader.read_line(&mut line); // DONE
                        server_log
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((n, "DONE".into()));
                        let _ = writer.write_all(format!("{tag} OK IDLE completed\r\n").as_bytes());
                        rejecting = matches!(script, IdleScript::ActivityThenReject);
                        after_done = matches!(
                            script,
                            IdleScript::ExistsThenSelectNo | IdleScript::QuietSelectNo
                        );
                    } else {
                        let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
                    }
                    let _ = writer.flush();
                }
            }
        });
        (port, log, handle)
    }

    /// True once connection `conn` has issued a command that is not part of
    /// the IDLE handshake (i.e. a sync pass ran on it).
    fn sync_ran_on(log: &Mutex<Vec<(usize, String)>>, conn: usize) -> bool {
        log.lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(n, w)| *n == conn && !w.starts_with('@') && !HANDSHAKE.contains(&w.as_str()))
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    // #3445 (after an IDLE reconnect nothing catches up): mail that arrived
    // while the socket was dead produces no EXISTS on the new connection, so
    // the reconnect itself must owe one catch-up resync.
    #[test]
    fn a_reconnect_after_a_dead_idle_socket_runs_a_catch_up_resync() {
        let (port, log, server) = scripted_server(vec![IdleScript::HangUp, IdleScript::Quiet]);
        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(port));
        wait_for("a sync pass on the second connection", || {
            sync_ran_on(&log, 1)
        });
        // The very first connection owes nothing: Swift's own pass covers it.
        assert!(!sync_ran_on(&log, 0));
        supervisor.stop("acct");
        let _ = server.join();
    }

    // #3445 (a failed IDLE resync silently drops the wake): the pass for a
    // server-announced wake fails; the wake produced no further notification,
    // so the retry must come from the supervisor.
    #[test]
    fn a_failed_idle_resync_is_retried_on_the_next_connection() {
        let (port, log, server) =
            scripted_server(vec![IdleScript::ActivityThenReject, IdleScript::Quiet]);
        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(port));
        wait_for("a retried sync pass on the second connection", || {
            sync_ran_on(&log, 1)
        });
        supervisor.stop("acct");
        let _ = server.join();
    }

    // #3445 (review): a per-mailbox failure (SELECT INBOX answers NO) comes
    // back as Ok(report) with `error` set, not Err. It must still be retried
    // on a fresh connection, with growing backoff while it keeps failing.
    #[test]
    fn a_mailbox_level_resync_failure_is_retried_with_growing_backoff() {
        let (port, log, server) = scripted_server(vec![
            IdleScript::ExistsThenSelectNo,
            IdleScript::QuietSelectNo,
            IdleScript::Quiet,
        ]);
        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(port));
        wait_for("a sync pass on the third connection", || {
            sync_ran_on(&log, 2)
        });
        supervisor.stop("acct");
        let _ = server.join();
        let at: Vec<u128> = log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(_, w)| w.strip_prefix('@').and_then(|ms| ms.parse().ok()))
            .collect();
        assert_eq!(at.len(), 3, "exactly three connections: {at:?}");
        let (first, second) = (at[1] - at[0], at[2] - at[1]);
        assert!(
            second > first,
            "backoff must grow while the failure repeats: {first} ms then {second} ms"
        );
    }

    /// Minimal IDLE-capable conversation that never ends an IDLE on its own.
    fn serve_idle_forever(stream: std::net::TcpStream) {
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let _ = writer.write_all(b"* OK test ready\r\n");
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            let mut parts = trimmed.splitn(2, ' ');
            let tag = parts.next().unwrap_or("*").to_string();
            let command = parts.next().unwrap_or("").to_ascii_uppercase();
            if command.starts_with("CAPABILITY") {
                let _ = writer.write_all(b"* CAPABILITY IMAP4rev1 IDLE\r\n");
            } else if command.starts_with("SELECT") {
                let _ = writer.write_all(b"* 0 EXISTS\r\n");
            } else if command.starts_with("IDLE") {
                let _ = writer.write_all(b"+ idling\r\n");
                let _ = writer.flush();
                line.clear();
                let _ = reader.read_line(&mut line); // DONE
            } else if command.starts_with("LOGOUT") {
                let _ = writer.write_all(b"* BYE\r\n");
                let _ = writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
                return;
            }
            let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
            let _ = writer.flush();
        }
    }

    // #3445 (two concurrent IDLE starts orphan a thread): both starts used to
    // find nothing to stop, spawn, and overwrite each other's map entry; the
    // loser kept its own connection open forever.
    #[test]
    fn concurrent_starts_for_one_account_leave_exactly_one_thread() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("addr").port();
        let accepted = Arc::new(Mutex::new(Vec::<std::net::TcpStream>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (acc, stp) = (Arc::clone(&accepted), Arc::clone(&stop));
        let server = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stp.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).expect("blocking");
                        acc.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(stream.try_clone().expect("clone"));
                        workers.push(thread::spawn(move || serve_idle_forever(stream)));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
            for w in workers {
                let _ = w.join();
            }
        });
        let profile = test_profile();
        let supervisor = Arc::new(IdleSupervisor::new());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let starters: Vec<_> = (0..2)
            .map(|_| {
                let (sup, prof, bar) = (
                    Arc::clone(&supervisor),
                    Arc::clone(&profile),
                    Arc::clone(&barrier),
                );
                thread::spawn(move || {
                    bar.wait();
                    sup.start(prof, cfg_for(port));
                })
            })
            .collect();
        for s in starters {
            s.join().expect("starter");
        }
        wait_for("the surviving thread to idle", || {
            supervisor.is_running("acct")
        });
        // Give an orphan time to connect, then count sockets still open.
        thread::sleep(Duration::from_millis(500));
        let open = accepted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|s| {
                let _ = s.set_nonblocking(true);
                let mut b = [0u8; 1];
                // Open = still readable (data) or WouldBlock (quiet). Ok(0) is a
                // clean close and any other error (a reset) is a dead socket.
                let open = match s.peek(&mut b) {
                    Ok(n) => n > 0,
                    Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
                };
                // O_NONBLOCK lives on the shared open file description, so this
                // clone's flag also applied to the server's own socket: put it
                // back, or the server's read fails, its half drops while this
                // clone holds the fd open, and the client's LOGOUT waits for
                // the full 60 s read timeout (#3445 review blocker).
                let _ = s.set_nonblocking(false);
                open
            })
            .count();
        assert_eq!(open, 1, "exactly one IDLE connection may stay open");
        supervisor.stop_all();
        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
    }

    // #3445 (closing the profile blocks up to 20 s): Shutdown is only read
    // between idle_wait ticks, so the tick bounds stop() latency.
    #[test]
    fn stop_returns_within_a_tick_of_a_silent_idle_server() {
        let server = Server::start(true);
        let profile = test_profile();
        let supervisor = IdleSupervisor::new();
        supervisor.start(Arc::clone(&profile), cfg_for(server.port));
        wait_for("idle", || supervisor.is_running("acct"));
        let t = Instant::now();
        supervisor.stop_all();
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "stop took {:?}",
            t.elapsed()
        );
    }
}
