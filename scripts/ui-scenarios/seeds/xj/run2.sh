#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xj explorer second pass: run the given scenarios in up to 3 parallel lanes; results in runs/summary2.txt.
# Usage: run2.sh <run-number> <scenario>...
cd "$(dirname "$0")/../../.." || exit 2
n=$1; shift; out=build/explore/xj/runs
run() { scripts/ui-check.sh --no-build "$1" > "$out/$1.r$n.txt" 2>&1; echo "$1 run$n exit $?" >> "$out/summary2.txt"; }
i=0; for s in "$@"; do run "$s" & i=$((i+1)); if ((i % 3 == 0)); then wait; fi; done; wait
