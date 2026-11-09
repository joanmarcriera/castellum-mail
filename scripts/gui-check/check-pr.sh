#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# check-pr.sh — one command per GUI check: build a PR's head in that PR's own
# worktree (the main checkout never leaves `main`, so this script is always
# present), launch the Debug "Castellum Dev" app against a disposable copy of
# the debug profile, so a branch's migrations never touch the profile another
# check depends on.
#
# Usage (from the repo root):
#   scripts/gui-check/check-pr.sh <PR#> [--fresh]   build + launch a pull request
#   scripts/gui-check/check-pr.sh main              build + launch main on the debug profile
#   scripts/gui-check/check-pr.sh launch            relaunch the last build, no rebuild
#   scripts/gui-check/check-pr.sh stop              quit the Dev app
#   scripts/gui-check/check-pr.sh real-profile      read-only: migration level of the REAL profile
#
#   <PR#>   `gh pr view` resolves the head branch; the worktree that already
#           holds that branch (agents leave one under .claude/worktrees) is
#           fast-forwarded to origin, or a throwaway one is added under
#           build/pr<N>. The app is built there and launched with
#           CASTELLUM_PROFILE_DIR=~/.castellum-debug-profile-pr<N>, copied
#           from ~/.castellum-debug-profile the first time (accounts included);
#           --fresh throws the copy away and re-copies. The previous PR's
#           build output is deleted first (each one is ~6 GB).
#   main    `git pull --ff-only` on the main checkout, build, launch with
#           ~/.castellum-debug-profile.
#
# Extra environment is passed through to the app, e.g.
#   CASTELLUM_FAKE_MIGRATION_DELAY_MS=20000 scripts/gui-check/check-pr.sh 179
# The launch is always direct (never `open`/LaunchServices): LaunchServices
# ignores CASTELLUM_PROFILE_DIR; since S68 a Debug build then falls back to
# ~/Library/Application Support/CastellumNativeDev/profile, never the real one
#. `real-profile` below still reads the REAL CastellumNative one.
# Log: /tmp/castellum-debug.log.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE_PROFILE="$HOME/.castellum-debug-profile"
REAL_PROFILE="$HOME/Library/Application Support/CastellumNative/profile"
STATE="$ROOT/build/gui-check.state"       # "<worktree>\n<profile>" of the last launch
LOG=/tmp/castellum-debug.log

die() { echo "check-pr: $*" >&2; exit 1; }

stop_dev_app() {
  # Only an ad-hoc Debug build living in a DerivedData.noindex tree — never a
  # release Castellum that may be running with the real profile.
  pkill -f 'DerivedData\.noindex/.*/Castellum\.app/Contents/MacOS/Castellum' 2>/dev/null || true
  sleep 1
}

migration_level() {
  local db="$1/castellum.db"
  [ -f "$db" ] || { echo "no castellum.db in $1"; return; }
  sqlite3 "file:$db?mode=ro" \
    "select 'migration ' || max(id) || ' (' || (select name from _migrations order by id desc limit 1) || ')' from _migrations;"
}

# launch <worktree> <profile>
launch() {
  local wt="$1" profile="$2"
  mkdir -p "$profile"
  local app; app="$("$wt/scripts/build-macos.sh" app-path)"
  [ -x "$app/Contents/MacOS/Castellum" ] || die "no built app at $app — run a build first"
  stop_dev_app
  CASTELLUM_PROFILE_DIR="$profile" nohup "$app/Contents/MacOS/Castellum" >"$LOG" 2>&1 &
  disown
  mkdir -p "$ROOT/build"; printf '%s\n%s\n' "$wt" "$profile" >"$STATE"
  echo "launched $(git -C "$wt" log --oneline -1 | cut -c1-60) with profile $profile (log: $LOG)"
}

# clean_previous <current worktree>: drop the last PR build's output if it was elsewhere
clean_previous() {
  local prev; prev="$(sed -n 1p "$STATE" 2>/dev/null || true)"
  [ -n "$prev" ] && [ "$prev" != "$1" ] && [ "$prev" != "$ROOT" ] && [ -d "$prev" ] || return 0
  cargo clean --manifest-path "$prev/native/Cargo.toml" >/dev/null 2>&1 || true
  rm -rf "$prev/build/DerivedData.noindex"
  echo "cleaned previous build output in $prev"
}

build() { "$1/scripts/build-macos.sh" build >/tmp/castellum-gui-build.log 2>&1 || die "build failed — see /tmp/castellum-gui-build.log"; echo "build ok"; }

# worktree_for <branch> <PR#>: the worktree holding it, fast-forwarded; or a new detached one
worktree_for() {
  local branch="$1" wt
  wt="$(git -C "$ROOT" worktree list --porcelain | awk -v b="refs/heads/$branch" '$1=="worktree"{w=$2} $1=="branch"&&$2==b{print w}')"
  if [ -n "$wt" ]; then
    [ -z "$(git -C "$wt" status --porcelain)" ] || die "$wt has local changes — commit or discard them first"
    git -C "$wt" merge -q --ff-only "origin/$branch" || die "$wt cannot fast-forward to origin/$branch"
  else
    wt="$ROOT/build/pr$2"
    if [ -d "$wt" ]; then git -C "$wt" checkout -q --detach "origin/$branch"
    else git -C "$ROOT" worktree add -q --detach "$wt" "origin/$branch"; fi
  fi
  echo "$wt"
}

case "${1:-}" in
  main)
    git -C "$ROOT" checkout -q main && git -C "$ROOT" pull -q --ff-only origin main
    echo "checkout: $(git -C "$ROOT" log --oneline -1 | cut -c1-70)"
    clean_previous "$ROOT"; build "$ROOT"; launch "$ROOT" "$BASE_PROFILE" ;;
  launch)
    [ -s "$STATE" ] || die "nothing launched yet"
    launch "$(sed -n 1p "$STATE")" "$(sed -n 2p "$STATE")" ;;
  stop)
    stop_dev_app; echo "stopped" ;;
  real-profile)
    echo "real profile: $(migration_level "$REAL_PROFILE")"
    echo "debug profile: $(migration_level "$BASE_PROFILE")" ;;
  ''|-h|--help)
    sed -n '2,32p' "$0" ;;
  *)
    n="$1"; [[ "$n" =~ ^[0-9]+$ ]] || die "unknown command $n"
    branch="$(gh pr view "$n" --json headRefName -q .headRefName)" || die "PR #$n not found"
    git -C "$ROOT" fetch -q origin
    profile="$HOME/.castellum-debug-profile-pr$n"
    [ "${2:-}" = --fresh ] && rm -rf "$profile"
    if [ ! -d "$profile" ]; then
      [ -d "$BASE_PROFILE" ] || die "no $BASE_PROFILE yet — run 'check-pr.sh main' once and add the persona accounts"
      stop_dev_app                      # never copy a profile while the app holds its WAL
      cp -R "$BASE_PROFILE" "$profile"
      echo "profile: copied $BASE_PROFILE -> $profile ($(migration_level "$profile"))"
    fi
    wt="$(worktree_for "$branch" "$n")"
    echo "worktree: $wt @ $(git -C "$wt" log --oneline -1 | cut -c1-60)"
    clean_previous "$wt"; build "$wt"; launch "$wt" "$profile"
    echo "PR #$n ($branch). When done: scripts/gui-check/check-pr.sh main" ;;
esac
