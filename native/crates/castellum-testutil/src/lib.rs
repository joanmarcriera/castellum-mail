// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Test-only scratch directory that cleans up after itself.
//!
//! Usage: in a `#[cfg(test)]` module or an integration test, write
//! `castellum_testutil::scratch_root().join("castellum-foo-test-1.db")` instead
//! of `std::env::temp_dir().join(..)`. Everything under the returned directory
//! is removed when the test binary exits.
//!
//! Why not `tempfile::TempDir` per test: most helpers hand back a `Database`
//! (or a bare path) with no owner to hold a guard, and a process-wide root is
//! the smallest change that fixes ~120k leaked `castellum-*` entries in
//! `$TMPDIR`. Only a SIGKILLed test binary can still leave its own
//! `castellum-scratch-<pid>` directory behind.

use std::path::PathBuf;
use std::sync::OnceLock;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Directory this process removes at exit. Created on first call.
pub fn scratch_root() -> PathBuf {
    ROOT.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("castellum-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch root");
        // SAFETY: registers a plain `extern "C" fn()`; it only reads a
        // OnceLock that is already initialised by the time it can run.
        unsafe {
            libc::atexit(remove_root);
        }
        dir
    })
    .clone()
}

extern "C" fn remove_root() {
    if let Some(dir) = ROOT.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_created_and_stable() {
        let a = scratch_root();
        assert!(a.is_dir());
        assert_eq!(a, scratch_root());
        assert!(a.starts_with(std::env::temp_dir()));
    }
}
