#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xf explorer, session 2: reseed the session-2 accounts, then run the six session-2 scenarios in 2 lanes.
# Usage: batch2.sh <pass-label>   (logs: build/explore/xf/<label>-laneN.log)
cd "$(dirname "$0")/../../.." || exit 2
L="$1"; D=build/explore/xf
python3 $D/seed.py m o p q r s > $D/$L-seed.log 2>&1
(scripts/ui-check.sh --no-build xf-offline-queue-replays xf-defect-mark-all-read-ignores-search xf-defect-mark-all-read-unified-noop > $D/$L-lane1.log 2>&1) &
(scripts/ui-check.sh --no-build xf-read-unread-markall xf-trash-archive-restore-empty-race xf-readonly-again-holds-queued-delete > $D/$L-lane2.log 2>&1) &
wait; date > $D/$L-done
