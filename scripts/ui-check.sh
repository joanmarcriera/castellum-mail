#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# ui-check.sh — UIT-1 Layer C: run the REAL Debug app against a scenario and produce
# evidence (PNGs, tree.json, report.json) with no mouse and no Accessibility /
# Screen Recording grant. The app inspects and captures itself in-process.
#
# Usage:
#   scripts/ui-check.sh <scenario-name|path> ...  |  --all
#        [--no-build] [--keep-profile] [--audit] [--allow-skip] [--timeout S]
#   scripts/ui-check.sh --unit              # Layer A: XCTest layout snapshots + invariants
#   scripts/ui-check.sh --verify-release [App.app]   # no harness symbols in a Release build
#
# Exit code: worst per-scenario code. 0 pass · 1 invariant violations · 2 step error /
# timeout / invalid · 3 skipped (a scenario-declared optionalTarget is not on this branch;
# aggregate exits 3 unless --allow-skip, and it is printed loudly) · 64 unsafe env.
# Env: UI_CHECK_LANG (default en) / UI_CHECK_LOCALE (default en_GB) launch the app in another language, e.g. UI_CHECK_LANG=ca UI_CHECK_LOCALE=ca_ES.
# A typo'd target is never a skip: it is invalid (2).
#
# Evidence: build/ui-snapshots/<scenario>/<UTC-stamp>-<sha>/ {report.json, summary.txt,
# app.log, NN-<capture>/<window>.png, <window>.frame.png, tree.json}. Keeps the last 5 runs.
# Scenarios live in scripts/ui-scenarios/*.json (schema: apple/Sources/Castellum/App/UIHarness).
#
# Safety: the app is launched DIRECTLY (never `open`, which drops env), with a temp
# CASTELLUM_PROFILE_DIR and an in-memory Keychain (TestHost.isUIScript).
# UserDefaults: CFFIXED_USER_HOME does NOT isolate them (cfprefsd ignores it, verified
# 2026-09-30), so the run uses a COPY of the Debug app, re-signed ad-hoc, with bundle id
# uk.co.riera.castellum.uitest.<runid> (per run, so concurrent runs never share a domain or app
# copy): its defaults domain is a throwaway that is deleted before and after every run (only
# that exact domain), and Castellum Dev's domain is never touched. The app itself also refuses
# (exit 64, in CastellumApp.init, before any profile access) unless its bundle id is uitest.*. --audit proves it
# (Dev-domain hash, Keychain item count, real-profile mtimes before/after).
# The watchdog kills ONLY the PID it started.
# GreenMail is never taken down (parallel workers share it).
set -uo pipefail   # not -e: exit codes are aggregated by hand; every fallible step is checked explicitly

ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)" || { echo "ui-check: not in a git checkout" >&2; exit 2; }
cd "$ROOT" || { echo "ui-check: cannot cd to $ROOT" >&2; exit 2; }
SC_DIR="$ROOT/scripts/ui-scenarios"
OUT_ROOT="$ROOT/build/ui-snapshots"
BUILD=1 KEEP=0 AUDIT=0 ALLOW_SKIP=0 MODE=run TIMEOUT_EXTRA=30 scenarios=() verify_app=""

while (($#)); do
  case "$1" in
    --all) for f in "$SC_DIR"/*.json; do scenarios+=("$f"); done ;;
    --no-build) BUILD=0 ;;
    --keep-profile) KEEP=1 ;;
    --audit) AUDIT=1 ;;
    --allow-skip) ALLOW_SKIP=1 ;;
    --timeout) shift; TIMEOUT_EXTRA="$1" ;;
    --unit) MODE=unit ;;
    --verify-release) MODE=verify; if [[ "${2:-}" != "" && "${2:0:2}" != "--" ]]; then verify_app="$2"; shift; fi ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) if [[ -f "$1" ]]; then scenarios+=("$1"); elif [[ -f "$SC_DIR/$1.json" ]]; then scenarios+=("$SC_DIR/$1.json");
       else echo "ui-check: unknown scenario '$1'" >&2; exit 2; fi ;;
  esac
  shift
done

# ---- --unit: Layer A -------------------------------------------------------
if [[ $MODE == unit ]]; then
  mkdir -p "$OUT_ROOT/unit"
  TEST_RUNNER_CASTELLUM_UI_SNAPSHOT_DIR="$OUT_ROOT/unit" \
    exec "$ROOT/scripts/build-macos.sh" test -- \
      -only-testing:CastellumTests/UILayoutSnapshotTests -only-testing:CastellumTests/UIInvariantsTests \
      -only-testing:CastellumTests/UIScriptScenarioTests
fi

# ---- --verify-release ------------------------------------------------------
# Debug is the positive control: the same greps MUST match there, else the check is blind.
leak_hits() {  # <App.app> -> prints matches across every Mach-O in the bundle (empty = clean).
  # A Debug app keeps its code in Castellum.debug.dylib, so scan all executables and dylibs.
  local f
  while IFS= read -r f; do
    file -b "$f" | grep -q 'Mach-O' || continue
    { strings -a "$f" | grep -E 'CASTELLUM_UI_SCRIPT|CASTELLUM_UI_OUT|CASTELLUM_UI_VIRTUAL_SCREEN|uiScriptPresent|UIScriptRunner|UIProbeView' ;
      nm -m "$f" 2>/dev/null | grep -iE 'UIScript|UIHarness|UIInvariant|UIIdle|UICapture|UIViewTree|UIProbe|UIReading|UIAXDriver' ; } | sed "s|^|$(basename "$f"): |"
  done < <(find "$1/Contents" -type f \( -perm -u+x -o -name '*.dylib' \)) | sort -u
}
if [[ $MODE == verify ]]; then
  dbg="$("$ROOT/scripts/build-macos.sh" app-path)"
  [[ -d "$dbg" ]] || { echo "verify-release: Debug build missing ($dbg); run scripts/build-macos.sh build" >&2; exit 2; }
  n=$(leak_hits "$dbg" | wc -l | tr -d ' ')
  if ((n == 0)); then echo "verify-release: POSITIVE CONTROL FAILED - Debug binary shows no harness symbols; the check is blind" >&2; exit 2; fi
  echo "verify-release: positive control ok (Debug binary shows $n harness matches)"
  rel="${verify_app:-$("$ROOT/scripts/build-macos.sh" app-path --release)}"
  [[ -d "$rel" ]] || { echo "verify-release: Release build not found at $rel" >&2; exit 2; }
  hits=$(leak_hits "$rel")
  if [[ -n "$hits" ]]; then echo "verify-release: FAIL - Release binary leaks harness symbols:" >&2; echo "$hits" | head -20 >&2; exit 1; fi
  echo "verify-release: OK - Release app $rel has no harness symbols"; exit 0
fi

# ---- run scenarios ---------------------------------------------------------
((${#scenarios[@]})) || { echo "usage: ui-check.sh <scenario>... | --all | --unit | --verify-release" >&2; exit 2; }

if ((BUILD)); then
  blog=$(mktemp "${TMPDIR:-/tmp}/ui-check-build.XXXXXX") || { echo "ui-check: mktemp failed" >&2; exit 2; }
  "$ROOT/scripts/build-macos.sh" build >"$blog" 2>&1 || { tail -20 "$blog"; echo "ui-check: build failed" >&2; exit 2; }
  rm -f "$blog"
fi
SRC_APP="$("$ROOT/scripts/build-macos.sh" app-path)"
[[ -x "$SRC_APP/Contents/MacOS/Castellum" ]] || { echo "ui-check: no app at $SRC_APP (build first)" >&2; exit 2; }
# Private per-run copy with its own bundle id => its own defaults domain (see header).
RUN_ID="r$(date +%s)x$$"
UI_ID="uk.co.riera.castellum.uitest.$RUN_ID"
APP_ROOT="$ROOT/build/ui-app.noindex"
APP="$APP_ROOT/$RUN_ID/Castellum.app"
mkdir -p "$APP_ROOT/$RUN_ID" || { echo "ui-check: cannot create $APP_ROOT/$RUN_ID" >&2; exit 2; }
find "$APP_ROOT" -mindepth 1 -maxdepth 1 -type d -mtime +1 -exec rm -rf {} + 2>/dev/null   # stale runs only
reset_ui_defaults() { defaults delete "$UI_ID" >/dev/null 2>&1 || true; }   # exact per-run domain only
cleanup() { reset_ui_defaults; rm -rf "$APP_ROOT/$RUN_ID"; }
trap cleanup EXIT
rsync -a "$SRC_APP/" "$APP/" || { echo "ui-check: cannot copy the app" >&2; exit 2; }
rm -rf "$APP/Contents/PlugIns"   # Share Extension etc. are not needed and would need their own signing/ids
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $UI_ID" -c "Set :CFBundleName CastellumUITest" "$APP/Contents/Info.plist" \
  || { echo "ui-check: cannot retarget bundle id" >&2; exit 2; }
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || { echo "ui-check: ad-hoc re-sign failed" >&2; exit 2; }
BIN="$APP/Contents/MacOS/Castellum"

want_flag() { grep -q "\"$1\": *true" "$2"; }
need_greenmail=0; need_personas=0
for s in "${scenarios[@]}"; do want_flag greenmail "$s" && need_greenmail=1; want_flag personas "$s" && need_personas=1; done
if ((need_greenmail)); then
  if ! { nc -z 127.0.0.1 3143 && nc -z 127.0.0.1 3025; } 2>/dev/null; then
    echo "ui-check: GreenMail down; starting (never 'down' it: workers share it)"
    colima status >/dev/null 2>&1 || colima start >/dev/null 2>&1
    docker-compose -f "$ROOT/scripts/local-imap/docker-compose.yml" up -d >/dev/null 2>&1
    for _ in $(seq 1 30); do nc -z 127.0.0.1 3143 2>/dev/null && break; sleep 2; done
  fi
  { nc -z 127.0.0.1 3143 && nc -z 127.0.0.1 3025; } >/dev/null 2>&1 || { echo "ui-check: GreenMail unavailable" >&2; exit 2; }
  if ((need_personas)); then
    cid=$(docker ps -q --filter publish=3143 2>/dev/null | head -1)
    stamp="$OUT_ROOT/.personas-${cid:-unknown}"
    if [[ ! -f "$stamp" ]]; then
      # A failed seed used to be silent: scenarios then timed out waiting for mail that was never
      # delivered. The seeder needs nodemailer, which this repo does not vendor (no package.json).
      mkdir -p "$OUT_ROOT"; slog=$(mktemp "${TMPDIR:-/tmp}/ui-check-seed.XXXXXX")
      if node "$ROOT/scripts/local-imap/seed-personas.js" >"$slog" 2>&1; then touch "$stamp"; rm -f "$slog"
      else
        tail -5 "$slog" >&2; rm -f "$slog"
        echo "ui-check: persona seeding failed (see above). If nodemailer is missing: npm install --prefix <dir> nodemailer, then NODE_PATH=<dir>/node_modules scripts/ui-check.sh ..." >&2
        exit 2
      fi
    fi
  fi
fi

defaults_hash() { defaults export uk.co.riera.castellum.dev - 2>/dev/null | shasum | cut -c1-16; }
kc_count() { security dump-keychain 2>/dev/null | grep -c castellum; }
# Read-only: any file (db/wal included) under a real profile dir newer than the stamp taken before the run.
real_changed() { compgen -G "$HOME/Library/Application Support/CastellumNative*" >/dev/null || return 0
  find "$HOME"/Library/Application\ Support/CastellumNative* -newer "$1" 2>/dev/null | sort; }

worst=0; skipped=0; sha=$(git rev-parse --short HEAD)
for s in "${scenarios[@]}"; do
  name=$(basename "$s" .json)
  run="$OUT_ROOT/$name/$(date -u +%Y%m%dT%H%M%SZ)-$sha"; mkdir -p "$run"
  profile=$(mktemp -d "${TMPDIR:-/tmp}/castellum-uit.XXXXXX") && [[ -n "$profile" && -d "$profile" ]] \
    || { echo "ui-check: cannot create a temp profile dir" >&2; exit 2; }
  mkdir -p "$profile/home"
  reset_ui_defaults
  ((AUDIT)) && { d0=$(defaults_hash); k0=$(kc_count); stamp=$(mktemp "${TMPDIR:-/tmp}/ui-check-stamp.XXXXXX"); sleep 1; }
  tmo=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1])).get('timeoutMs',60000)//1000)" "$s")
  env \
  CASTELLUM_PROFILE_DIR="$profile" CASTELLUM_UI_SCRIPT="$s" CASTELLUM_UI_OUT="$run" \
  CFFIXED_USER_HOME="$profile/home" \
    "$BIN" -NSAutomaticWindowAnimationsEnabled NO -NSWindowResizeTime 0.001 \
      -ApplePersistenceIgnoreState YES -AppleLanguages "(${UI_CHECK_LANG:-en})" -AppleLocale "${UI_CHECK_LOCALE:-en_GB}" \
      >"$run/app.log" 2>&1 &
  pid=$!
  ( sleep $((tmo + TIMEOUT_EXTRA)); kill -TERM "$pid" 2>/dev/null && { sleep 5; kill -KILL "$pid" 2>/dev/null; } ) 2>/dev/null &
  guard=$!
  wait "$pid"; code=$?
  kill "$guard" 2>/dev/null; wait "$guard" 2>/dev/null
  ui_keys=$(defaults read "$UI_ID" 2>/dev/null | grep -c ' = ' || true)
  reset_ui_defaults
  echo "== $name: exit $code"
  [[ -f "$run/summary.txt" ]] && cat "$run/summary.txt" || echo "(no summary.txt written; see $run/app.log)"
  if ((AUDIT)); then
    d1=$(defaults_hash); k1=$(kc_count); changed=$(real_changed "$stamp"); rm -f "$stamp"
    echo "audit: castellum-dev defaults hash $d0 -> $d1 $([[ $d0 == "$d1" ]] && echo unchanged || echo CHANGED)"
    echo "audit: keychain 'castellum' items $k0 -> $k1 $([[ $k0 == "$k1" ]] && echo unchanged || echo CHANGED)"
    echo "audit: real profile files newer than run start: $([[ -z $changed ]] && echo none '(unchanged)' || { echo CHANGED; echo "$changed" | head -5; })"
    echo "audit: throwaway defaults domain $UI_ID held $ui_keys keys during the run (deleted after)"
    [[ $d0 == "$d1" && $k0 == "$k1" && -z $changed ]] || code=2
  fi
  ((KEEP)) || rm -rf "$profile"
  ls -1dt "$OUT_ROOT/$name"/*/ 2>/dev/null | tail -n +6 | xargs -I{} rm -rf {}
  if ((code == 3)); then
    skipped=$((skipped + 1))
    echo "!!!! SKIPPED: $name did NOT run (target not on this branch). This is not a pass. !!!!" >&2
  elif ((code > worst)); then worst=$code; fi
done
((worst > 0)) && exit $worst
if ((skipped > 0 && ! ALLOW_SKIP)); then echo "ui-check: $skipped scenario(s) skipped; exiting 3 (use --allow-skip to accept)" >&2; exit 3; fi
exit 0
