#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Lists (and, with --apply, deletes) stale Castellum test-profile items left
# in the REAL login keychain by interrupted `xcodebuild test` runs — before
# the fix (AccountStore.init defaulting to an in-memory
# Keychain backing under TestHost.isActive), unit tests wrote real SecItem*
# entries under a per-profile namespace derived from a temp directory
# (AccountStore.service(forProfileDirectory:)), and a hung/killed teardown
# left them behind instead of deleting them. Earlier builds also leaked a
# second namespace ("com.castellum.license.profile.*"); it is still swept so
# old leftovers can be cleaned from a developer keychain.
#
# Scope is deliberately narrow: ONLY "<service>.profile.*" namespaces, which
# by construction are never used by the shipped app (that always uses the
# bare service, e.g. "com.castellum.account", or a
# CASTELLUM_PROFILE_DIR smoke, which gets its own profile.* namespace too but
# is Marc's own manual run — check before applying if you've been running
# those). This script NEVER touches a bare (non-profile.*) service.
#
# Usage:
#   scripts/keychain-test-sweep.sh                       # dry run, both stores
#   scripts/keychain-test-sweep.sh --apply                # delete, both stores
#   scripts/keychain-test-sweep.sh --prefix com.castellum.account.profile.   # one store only
#   scripts/keychain-test-sweep.sh --apply --prefix com.castellum.license.profile.
set -euo pipefail

APPLY=0
PREFIXES=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply)
      APPLY=1
      shift
      ;;
    --prefix)
      [[ -n "${2:-}" ]] || { echo "usage: $0 [--apply] [--prefix <service.profile.>]..." >&2; exit 1; }
      # Safety invariant (see header): this script must NEVER touch a bare
      # service — only "<service>.profile.*" test namespaces. Enforce it here
      # too, not just by convention, so a caller typo (e.g. forgetting the
      # trailing ".profile.") can't turn --apply into a production-data delete.
      [[ "$2" == *.profile.* ]] || { echo "error: --prefix must contain '.profile.' (got '$2') — refusing to risk matching a bare production service" >&2; exit 1; }
      PREFIXES+=("$2")
      shift 2
      ;;
    *)
      echo "usage: $0 [--apply] [--prefix <service.profile.>]..." >&2
      exit 1
      ;;
  esac
done

# Default: sweep both known test-namespaced stores (AccountStore, and the legacy licence namespace).
if [[ ${#PREFIXES[@]} -eq 0 ]]; then
  PREFIXES=(
    "com.castellum.account.profile."
    "com.castellum.license.profile."
  )
fi

# `security dump-keychain` lists attributes (service, account, dates) without
# decrypting secret data, so it needs no unlock/auth prompt. "svce" is the
# generic-password service attribute.
all_svce="$(security dump-keychain 2>/dev/null \
  | grep '"svce"<blob>=' \
  | sed -E 's/.*"svce"<blob>="([^"]*)".*/\1/' \
  | sort -u || true)"

total=0
for PREFIX in "${PREFIXES[@]}"; do
  services="$(echo "$all_svce" | grep "^${PREFIX}" || true)"

  if [[ -z "$services" ]]; then
    echo "No stale ${PREFIX}* items found."
    continue
  fi

  count="$(echo "$services" | wc -l | tr -d ' ')"
  total=$((total + count))
  echo "Found $count stale service namespace(s) under ${PREFIX}*:"
  echo "$services"
  echo

  if [[ "$APPLY" -eq 0 ]]; then
    continue
  fi

  while IFS= read -r svc; do
    [[ -z "$svc" ]] && continue
    echo "Deleting all items for service: $svc"
    # A profile namespace can hold >1 account; delete-generic-password
    # removes one matching item per call, so loop until none remain.
    while security delete-generic-password -s "$svc" >/dev/null 2>&1; do :; done
  done <<<"$services"
done

if [[ "$total" -eq 0 ]]; then
  exit 0
fi

if [[ "$APPLY" -eq 0 ]]; then
  echo "Dry run only — nothing deleted. Re-run with --apply to delete these."
else
  echo "Done. Deleted items for $total service namespace(s) total."
fi
