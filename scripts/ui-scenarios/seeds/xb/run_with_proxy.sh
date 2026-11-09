#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Usage: run_with_proxy.sh <scenario>  — starts flaky_smtp_proxy.py (127.0.0.1:3925, first connection
# dropped), runs the scenario with --no-build, prints the proxy's connection log, stops the proxy.
here="$(cd "$(dirname "$0")" && pwd)"; root="$here/../../.."
python3 "$here/flaky_smtp_proxy.py" 3925 1 > "$here/proxy.log" 2>&1 & pp=$!
sleep 1
(cd "$root" && scripts/ui-check.sh --no-build "$1" 2>&1 | tail -8)
kill $pp 2>/dev/null; echo "--- proxy log"; cat "$here/proxy.log"
