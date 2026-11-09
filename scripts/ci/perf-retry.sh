#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Run a perf-budget command; if it fails, wait and run it ONCE more. The step
# fails only when both attempts fail.
#
# Usage: scripts/ci/perf-retry.sh <command> [args...]
#   PERF_RETRY_WAIT  seconds to wait before the second attempt (default 60)
#   PERF_RETRY_MARK  file touched when attempt 2 rescued the step
#                    (default build/perf-retry-rescued, relative to the cwd);
#                    the workflow's success report adds "(perf passed on retry)"
#                    to the GitHub status so a flaky pass is visible on the PR.
#
# Why (2026-10-17, CI investigation on 7f50973e / bd2dc0c3): macbook-runner is
# Marc's own MacBook, which runs many Claude/agent sessions at once (load
# averages 17-150 on 15 cores during the day). The perf gates' blocking
# CEILINGS are there to catch gross regressions (O(n^2), a 2x slowdown), but a
# single attempt under a load burst tripped a different row each time
# (qresync_noop_resync 620 ms with p50 330 ms; counts_all_accounts 113 ms;
# follow-up rebuild_threads p95 261 ms) while the same rows passed on the
# neighbouring runs. A real regression fails both attempts; a load burst
# usually does not. Each attempt prints `uptime` so the log shows the load it
# ran under, and a retry is announced loudly so it is never silent.
set -uo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

wait_s="${PERF_RETRY_WAIT:-60}"
mark="${PERF_RETRY_MARK:-build/perf-retry-rescued}"

echo "perf-retry: attempt 1 of 2 ($(uptime))"
"$@"
status=$?
if [ "$status" -eq 0 ]; then
  exit 0
fi
# Killed by a signal (job cancelled, timeout): never retry, just propagate.
if [ "$status" -gt 128 ]; then
  exit "$status"
fi

echo "::warning::perf-retry: attempt 1 failed (exit $status); waiting ${wait_s}s and running once more"
sleep "$wait_s"
echo "perf-retry: attempt 2 of 2 ($(uptime))"
"$@"
status=$?
if [ "$status" -eq 0 ]; then
  mkdir -p "$(dirname "$mark")" && touch "$mark"
  echo "::warning::perf-retry: passed on attempt 2 - attempt 1 was over a ceiling (load burst?); see its rows above"
  exit 0
fi
echo "perf-retry: failed on both attempts (exit $status) - treat as a real regression" >&2
exit "$status"
