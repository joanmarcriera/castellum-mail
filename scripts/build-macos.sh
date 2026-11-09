#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# build-macos.sh — THE canonical xcodebuild invocation for Castellum.
# Agents and humans: use this instead of hand-rolling xcodebuild lines, so every
# build lands in one predictable, bounded DerivedData location per checkout.
#
# Usage:
#   scripts/build-macos.sh [test|build|app-path] [--release] [--skip-rust] [--skip-gen]
#                          [-- extra xcodebuild args...]
#
#   test       (default) Debug, CODE_SIGNING_ALLOWED=NO, run XCTest — same as CI.
#   build      Build the app. Debug = ad-hoc signed "Castellum Dev"
#              (uk.co.riera.castellum.dev); --release = Developer ID signed
#              production id (uk.co.riera.castellum), host arch only.
#   app-path   Print where the built Castellum.app is (no build).
#   --skip-rust  Don't rebuild libcastellum_ffi.a (apple/scripts/build-rust.sh).
#   --skip-gen   Don't re-run xcodegen.
#   Everything after `--` is passed to xcodebuild verbatim
#   (e.g. -- -only-testing:CastellumTests/SharePendingComposeTests).
#
# DerivedData strategy (see CLAUDE.md "macOS build, signing & TCC"):
#   <checkout root>/build/DerivedData.noindex — one per checkout/worktree.
#   * Per-worktree so concurrent agent builds never share (and corrupt) one tree.
#   * Inside the checkout, so `git worktree remove --force` deletes it too:
#     DerivedData lives exactly as long as the worktree — no orphans in
#     ~/Library/Developer/Xcode/DerivedData (122 of them on 2026-09-23).
#   * `.noindex` keeps Spotlight — and so Launch Services' mailto:/bundle-id
#     lookups — from cataloguing every build as another "Castellum.app".
#   * build/* is already gitignored.
#   Override only for a reason: CASTELLUM_DERIVED_DATA=/path scripts/build-macos.sh
#
# Output is filtered through scripts/ci/xcodebuild-quiet.sh (errors + verdict,
# full log path printed last). Exit status is xcodebuild's.
set -euo pipefail

ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
DERIVED="${CASTELLUM_DERIVED_DATA:-$ROOT/build/DerivedData.noindex}"

action=test config=Debug skip_rust=0 skip_gen=0 extra=()
while (($#)); do
  case "$1" in
    test|build|app-path) action="$1" ;;
    --release) config=Release ;;
    --skip-rust) skip_rust=1 ;;
    --skip-gen) skip_gen=1 ;;
    --) shift; extra=("$@"); break ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "build-macos.sh: unknown argument '$1' (see --help)" >&2; exit 2 ;;
  esac
  shift
done

APP="$DERIVED/Build/Products/$config/Castellum.app"
if [[ "$action" == app-path ]]; then echo "$APP"; exit 0; fi
if [[ "$action" == test && "$config" == Release ]]; then
  echo "build-macos.sh: tests run in Debug only (the scheme's test action)" >&2; exit 2
fi

cd "$ROOT"
if ((!skip_rust)); then bash apple/scripts/build-rust.sh; fi
if ((!skip_gen)); then xcodegen generate --quiet --spec apple/project.yml; fi

args=(-project apple/Castellum.xcodeproj -scheme Castellum -configuration "$config"
      -destination 'platform=macOS' -derivedDataPath "$DERIVED")
sign=()
if [[ "$action" == test ]]; then
  sign=(CODE_SIGNING_ALLOWED=NO)          # identical to the CI apple-build job
elif [[ "$config" == Release ]]; then
  # The team is read from the Developer ID identity in the login keychain
  # (the repo deliberately carries no account identity); it makes
  # $(TeamIdentifierPrefix) expand, so the App Group is the authorized one.
  team="${CASTELLUM_TEAM_ID:-$(security find-identity -v -p codesigning \
    | sed -nE 's/.*"Developer ID Application: .*\(([A-Z0-9]{10})\)".*/\1/p' | head -1)}"
  [[ -n "$team" ]] || { echo "build-macos.sh: no Developer ID Application identity found" >&2; exit 1; }
  # Host arch only: a local check, not a distributable (release.yml builds universal).
  sign=(DEVELOPMENT_TEAM="$team" ONLY_ACTIVE_ARCH=YES "ARCHS=$(uname -m)")
fi
# Debug build: signing comes from configs/Direct.xcconfig (ad-hoc). Never add
# CODE_SIGN_IDENTITY / bundle-id overrides here — they belong in the xcconfigs.

status=0
bash scripts/ci/xcodebuild-quiet.sh "${args[@]}" "${sign[@]}" ${extra[@]+"${extra[@]}"} "$action" || status=$?
[[ "$action" == build && $status == 0 ]] && echo "app: $APP"
exit $status
