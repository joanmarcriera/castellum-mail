// Castellum - opt-in per-phase timing for the sync pass (perf baseline).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Records how long each stage of `sync::sync_with_provider` takes, so a
// benchmark (`examples/sync_bench.rs`) can attribute a first sync's wall time
// to IMAP round trips, SQLite writes, reconciliation, threading and rules.
// See project-docs/perf/2026-09-sync-baseline.md for the numbers it produced.
//
// Off by default and designed to cost nothing measurable when off: the sink
// is a thread-local `Option`, so a disabled `time()` is one TLS read plus a
// branch — no clock read, no allocation. Only a caller that explicitly
// `enable()`s it on its own thread pays for `Instant::now()` and a `Vec` push
// per sample. Thread-local (not global) because a sync pass runs entirely on
// the calling thread, and it keeps concurrent syncs for other accounts (or
// parallel tests) from polluting each other's samples.
//
// This is a measurement aid, not product telemetry: nothing here is
// persisted or crosses the C ABI.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Raw samples per phase, in microseconds, in the order they were recorded.
#[derive(Debug, Default, Clone)]
pub struct SyncTimings {
    pub samples: BTreeMap<&'static str, Vec<u64>>,
}

impl SyncTimings {
    /// Sum of every sample recorded for `phase`, in microseconds.
    pub fn total_us(&self, phase: &str) -> u64 {
        self.samples.get(phase).map_or(0, |v| v.iter().sum())
    }
}

thread_local! {
    static SINK: RefCell<Option<SyncTimings>> = const { RefCell::new(None) };
}

/// Start recording on the current thread (discarding anything recorded before).
pub fn enable() {
    SINK.with(|s| *s.borrow_mut() = Some(SyncTimings::default()));
}

/// Stop recording on the current thread and return what was collected, or
/// `None` if recording was never enabled.
pub fn take() -> Option<SyncTimings> {
    SINK.with(|s| s.borrow_mut().take())
}

/// Whether recording is currently on for THIS thread. Lets a caller that
/// wants a look at the samples (e.g. to persist a KPI snapshot after a sync)
/// tell whether it would be the one starting/stopping recording, or whether
/// an outer caller (a bench harness, a perf-gate test) already owns that
/// lifecycle and still expects its own `take()` to see everything.
pub fn is_enabled() -> bool {
    SINK.with(|s| s.borrow().is_some())
}

/// Clone the samples recorded so far on this thread WITHOUT clearing them —
/// unlike [`take`], recording keeps running and an outer `take()` still sees
/// every sample. `None` if recording is off. Use this when you need to look
/// at the data mid-flight without taking over the enable/take lifecycle from
/// whoever started it.
pub fn snapshot() -> Option<SyncTimings> {
    SINK.with(|s| s.borrow().clone())
}

/// Record one sample for `phase`. No-op unless enabled on this thread.
pub(crate) fn record(phase: &'static str, elapsed: Duration) {
    SINK.with(|s| {
        if let Some(t) = s.borrow_mut().as_mut() {
            t.samples
                .entry(phase)
                .or_default()
                .push(elapsed.as_micros() as u64);
        }
    });
}

/// For spans that don't fit a closure: `Some(now)` when enabled, else `None`
/// (the clock is never read while disabled). Pair with [`record_since`].
pub(crate) fn now() -> Option<Instant> {
    SINK.with(|s| s.borrow().is_some()).then(Instant::now)
}

/// Record the time since a [`now`] mark under `phase` (no-op for `None`).
pub(crate) fn record_since(phase: &'static str, start: Option<Instant>) {
    if let Some(start) = start {
        record(phase, start.elapsed());
    }
}

/// Run `f`, recording its duration under `phase` when enabled. When disabled
/// the clock is never read.
pub(crate) fn time<T>(phase: &'static str, f: impl FnOnce() -> T) -> T {
    if !SINK.with(|s| s.borrow().is_some()) {
        return f();
    }
    let start = Instant::now();
    let out = f();
    record(phase, start.elapsed());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_records_nothing_and_enabled_collects_samples() {
        assert!(take().is_none());
        assert_eq!(time("p", || 7), 7);
        assert!(take().is_none(), "disabled time() must not create a sink");

        enable();
        time("p", || ());
        time("p", || ());
        time("q", || ());
        let t = take().expect("enabled");
        assert_eq!(t.samples["p"].len(), 2);
        assert_eq!(t.samples["q"].len(), 1);
        assert!(take().is_none(), "take() disables recording");
    }

    #[test]
    fn snapshot_peeks_without_clearing() {
        assert!(!is_enabled());
        assert!(snapshot().is_none());

        enable();
        assert!(is_enabled());
        time("p", || ());
        let peek = snapshot().expect("enabled");
        assert_eq!(peek.samples["p"].len(), 1);
        assert!(is_enabled(), "snapshot() must not stop recording");

        // The outer owner's take() still sees everything, including the
        // sample recorded before the snapshot.
        time("p", || ());
        let t = take().expect("still enabled after snapshot()");
        assert_eq!(t.samples["p"].len(), 2);
        assert!(!is_enabled());
    }
}
