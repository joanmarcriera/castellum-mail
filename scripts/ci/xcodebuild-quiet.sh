#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xcodebuild-quiet.sh — run xcodebuild with the full log on disk and only the
# decisive lines on stdout. Raw xcodebuild prints 50–130k characters per run,
# which an agent then carries in its context for every later turn.
#
# Usage: scripts/ci/xcodebuild-quiet.sh <xcodebuild args...>
#   e.g. scripts/ci/xcodebuild-quiet.sh -project apple/Castellum.xcodeproj -scheme Castellum \
#          -configuration Debug -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO test
# Output: errors, failed tests, warnings count, and the "** TEST SUCCEEDED/FAILED **" or
#         "** BUILD SUCCEEDED/FAILED **" line. Full log path is printed last.
# Exit:   xcodebuild's own exit status.
set -uo pipefail
log="${XCODEBUILD_LOG:-$(mktemp -t xcodebuild.XXXXXX).log}"
xcodebuild "$@" >"$log" 2>&1
status=$?
# Errors and failed tests: de-duplicated and capped, since one bad file can
# repeat an error hundreds of times.
grep -E '\.swift:[0-9]+:[0-9]+: error:|error: |Test Case .* failed' "$log" \
  | grep -vE 'linkd.*Code=4097|Code=4097.*linkd' | sort -u | head -60
# The overall test total and the verdict are printed on their own, never through
# the cap above: per-class "Executed N tests" lines start with a tab, which sorts
# before "**", so with 60+ test classes `sort -u | head` used to drop the verdict.
grep -E 'Executed [0-9]+ tests' "$log" | tail -1
grep -E '\*\* (TEST|BUILD) (SUCCEEDED|FAILED) \*\*' "$log" | tail -1
printf 'warnings: %s\n' "$(grep -c ': warning:' "$log")"
printf 'full log: %s\n' "$log"
exit $status
