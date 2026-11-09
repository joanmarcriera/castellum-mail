#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xj explorer: run the kept scenarios (pass set as run N, defect set as run M), two lanes in parallel.
# Usage: runall.sh <pass-run-number> <defect-run-number|skip>
cd "$(dirname "$0")/../../.." || exit 2
out=build/explore/xj/runs; mkdir -p "$out"
run() { scripts/ui-check.sh --no-build "$1" > "$out/$1.$2.txt" 2>&1; echo "$1 run$2 exit $?" >> "$out/summary.txt"; }
P=(xj-mute-thread-survives-reload xj-snooze-resnooze-unsnooze xj-tag-smart-mailbox-conditions xj-correspondence-thread-and-empty xj-unsubscribe-sheet-destination-and-cancel xj-projects-board-long-name xj-tags-settings-long-name-and-server-keywords)
D=(xj-defect-tags-vanish-on-mailbox-reload xj-defect-user-tag-named-junk-invisible xj-defect-unsubscribe-https-only-dead-end xj-defect-unsubscribe-refused-leaves-draft)
( for s in "${P[@]:0:4}"; do run "$s" "$1"; done ) &
( for s in "${P[@]:4}"; do run "$s" "$1"; done ) &
[ "$2" != skip ] && ( for s in "${D[@]}"; do run "$s" "$2"; done ) &
wait
