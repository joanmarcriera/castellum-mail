#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xf explorer: reseed the writable xf accounts, then run every kept scenario in 3 lanes.
# Usage: batch.sh <pass-label>   (logs: build/explore/xf/<label>-laneN.log, marker <label>-done)
cd "$(dirname "$0")/../../.." || exit 2
L="$1"; D=build/explore/xf
python3 $D/seed.py a b c d e f g h i j k > $D/$L-seed.log 2>&1
(scripts/ui-check.sh --no-build xf-defect-readonly-refusal-silent xf-readonly-never-half-applies xf-archive-delete-single xf-defect-read-flag-lost-on-move > $D/$L-lane1.log 2>&1) &
(scripts/ui-check.sh --no-build xf-trash-permanent-delete xf-unified-two-accounts xf-defect-delete-trashed-from-search > $D/$L-lane2.log 2>&1) &
(scripts/ui-check.sh --no-build xf-defect-permanent-prompt-for-inbox-message xf-awkward-twice-switch-min xf-defect-archived-mail-vanishes > $D/$L-lane3.log 2>&1) &
wait; date > $D/$L-done
