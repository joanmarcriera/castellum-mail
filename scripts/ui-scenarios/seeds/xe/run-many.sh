#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xe explorer: for each scenario (name in scripts/ui-scenarios or a path), re-seed the xe-*
# GreenMail accounts, run it with --no-build, and print the server truth afterwards.
# Usage: build/explore/xe/run-many.sh <scenario> [<scenario> ...]    (log: build/explore/xe/logs/<name>.log)
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"; cd "$ROOT" || exit 2
mkdir -p build/explore/xe/logs; worst=0
for sc in "$@"; do
  n="$(basename "$sc" .json)"
  { python3 build/explore/xe/seed.py reset && scripts/ui-check.sh --no-build "$sc"; rc=$?
    echo "--- server after $n (rc=$rc)"; python3 build/explore/xe/seed.py dump xe-r1@local.test xe-r2@local.test
    d="$(ls -d build/ui-snapshots/$n/2* 2>/dev/null | tail -1)"
    [ -n "$d" ] && python3 - "$d/report.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]))
print("status",r.get("status"),"lastStep",r.get("lastStep"),"error",r.get("error"))
for v in r.get("violations",[]) if isinstance(r.get("violations"),list) else []: print("  V",v.get("rule"),v.get("window"),v.get("element"),"::",v.get("detail"))
PY
  } > "build/explore/xe/logs/$n.log" 2>&1
  rc=$(grep -o 'rc=[0-9]*' "build/explore/xe/logs/$n.log" | head -1 | cut -d= -f2); echo "$n rc=${rc:-?}"
  [ "${rc:-2}" -gt "$worst" ] && worst=${rc:-2}
done
echo "ALL-DONE worst=$worst"
