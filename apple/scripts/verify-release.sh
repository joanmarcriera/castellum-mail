#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# The release gate: prove a built app+DMG are actually signed, notarized,
# stapled, universal, and accepted by Gatekeeper.
#
#   bash apple/scripts/verify-release.sh <path/to/Castellum.app> <path/to/App.dmg>
#
# Exists as a script, not inline workflow YAML, so the same gate can be run by
# hand on an artefact. "The workflow went green" is not evidence — these
# commands are.
set -euo pipefail

APP="${1:?usage: verify-release.sh <App.app> <App.dmg>}"
DMG="${2:?usage: verify-release.sh <App.app> <App.dmg>}"
fail=0
check() { echo "--- $* ---"; "$@" || { echo "FAILED: $*" >&2; fail=1; }; }

# Universal: an arm64-only binary links and runs fine on the Apple Silicon
# runner that built it, and is simply broken on Intel. Check before anything
# else, because every later check would pass on a broken build.
echo "--- architectures ---"
binary="$APP/Contents/MacOS/$(basename "${APP%.app}")"
archs="$(lipo -archs "$binary" 2>/dev/null || echo "?")"
echo "$binary: $archs"
for want in arm64 x86_64; do
  grep -qw "$want" <<<"$archs" || { echo "FAILED: binary is missing $want" >&2; fail=1; }
done

check codesign --verify --strict --deep --verbose=2 "$APP"
# The DMG is a signed object in its own right. hdiutil does not sign it, so
# this fails loudly if the signing step is ever dropped from the workflow —
# the app inside passing tells you nothing about the wrapper.
check codesign --verify --strict --verbose=2 "$DMG"
# Gatekeeper's real verdict — codesign passing does not imply this passes.
check spctl -a -vvv --type exec "$APP"
# Staple the DMG too: stapling only the .app leaves a DMG that warns on first
# open for anyone who downloads it before Apple's CDN has the ticket.
check xcrun stapler validate "$APP"
check xcrun stapler validate "$DMG"

if [[ "$fail" != "0" ]]; then
  echo "verify-release: FAILED — do not publish this build." >&2
  exit 1
fi
echo "verify-release: all checks passed."
