#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# diagnose-macos-permissions.sh — READ-ONLY report on a Castellum.app's identity,
# signature, entitlements and TCC-relevant state. Modifies nothing (no tccutil,
# no lsregister, no deletes) — safe to run any time.
#
# Usage:
#   scripts/diagnose-macos-permissions.sh [path/to/Castellum.app ...] [--tcc-log]
#     No path: this checkout's canonical Debug and Release builds (if built) and
#     /Applications/Castellum.app.
#     --tcc-log: also scan the last 24h of tccd's log for Castellum prompts
#     (slow, ~1 min; read-only `log show`).
#
# What to look for:
#   * Debug build: Identifier uk.co.riera.castellum.dev, "Castellum Dev",
#     Signature=adhoc, no TeamIdentifier, App Group verdict "handoff OFF".
#   * Release build: uk.co.riera.castellum, Developer ID Application authority,
#     TeamIdentifier TEAMID0000, runtime flag, App Group "TEAMID0000.uk.co.riera.
#     castellum" verdict "authorized".
#   * NSAppleEventsUsageDescription / automation entitlement ABSENT — Castellum
#     sends no Apple Events (see CLAUDE.md for when that would change).
#   * "access data from other apps" prompt = TCC service SystemPolicyAppData
#     (App Group / other-app container access), NOT AppleEvents (Automation).
set -uo pipefail

ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || pwd)"
tcc_log=0 apps=()
for a in "$@"; do
  case "$a" in
    --tcc-log) tcc_log=1 ;;
    -h|--help) sed -n '2,24p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) apps+=("$a") ;;
  esac
done
if ((${#apps[@]} == 0)); then
  for c in Debug Release; do apps+=("$ROOT/build/DerivedData.noindex/Build/Products/$c/Castellum.app"); done
  apps+=(/Applications/Castellum.app)
fi

plist() { /usr/libexec/PlistBuddy -c "Print :$2" "$1/Contents/Info.plist" 2>/dev/null || echo "(absent)"; }

report() {
  local app="$1"
  echo "================ $app"
  [[ -d "$app" ]] || { echo "  not built / not present"; return; }
  local exe; exe="$app/Contents/MacOS/$(plist "$app" CFBundleExecutable)"
  echo "  executable:            $exe"
  echo "  CFBundleIdentifier:    $(plist "$app" CFBundleIdentifier)"
  echo "  CFBundleName:          $(plist "$app" CFBundleName)"
  echo "  CFBundleDisplayName:   $(plist "$app" CFBundleDisplayName)"
  echo "  version:               $(plist "$app" CFBundleShortVersionString) ($(plist "$app" CFBundleVersion))"
  echo "  NSAppleEventsUsageDescription: $(plist "$app" NSAppleEventsUsageDescription)"
  echo "  -- codesign -dv"
  codesign -dv --verbose=4 "$app" 2>&1 \
    | grep -E '^(Identifier|Format|CodeDirectory|Signature|Authority|TeamIdentifier|CDHash|Runtime Version|Timestamp)' \
    | sed 's/^/    /'
  local flags; flags="$(codesign -dv "$app" 2>&1 | sed -nE 's/.*flags=(0x[0-9a-f]+\([^)]*\)).*/\1/p')"
  echo "  flags:                 ${flags:-?}   (runtime = Hardened Runtime on)"
  echo "  -- designated requirement"
  codesign -d -r- "$app" 2>&1 | grep -v '^Executable' | sed 's/^/    /'
  echo "  -- codesign --verify --deep --strict"
  if out="$(codesign --verify --deep --strict --verbose=2 "$app" 2>&1)"; then
    echo "    OK"; else echo "$out" | sed 's/^/    /'; fi
  echo "  -- effective entitlements"
  local ents; ents="$(codesign -d --entitlements - --xml "$app" 2>/dev/null)"
  if [[ -n "$ents" ]]; then plutil -p - <<<"$ents" 2>/dev/null | sed 's/^/    /'; else echo "    (none)"; fi
  if grep -q 'com.apple.security.automation.apple-events' <<<"$ents"; then
    echo "    automation.apple-events: PRESENT"; else echo "    automation.apple-events: absent"; fi
  # App Group verdict: mirrors SharePendingCompose.authorizedAppGroupId.
  local team; team="$(codesign -dv "$app" 2>&1 | sed -n 's/^TeamIdentifier=//p')"
  [[ "$team" == "not set" ]] && team=""
  local want="${team}.uk.co.riera.castellum"
  if [[ -n "$team" ]] && grep -q "<string>$want</string>" <<<"$ents"; then
    echo "  App Group:             $want — authorized by Team ID (no prompt)"
  elif grep -q 'application-groups' <<<"$ents"; then
    echo "  App Group:             entitled but NOT authorizable (team='${team:-none}') — handoff OFF, container never opened"
  else
    echo "  App Group:             no entitlement — handoff OFF"
  fi
  [[ -e "$app/Contents/embedded.provisionprofile" ]] && echo "  embedded.provisionprofile: present" \
    || echo "  embedded.provisionprofile: none (expected for Direct/Developer ID)"
  local ext="$app/Contents/PlugIns/Castellum Share.appex"
  [[ -d "$ext" ]] && echo "  Share Extension id:    $(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$ext/Contents/Info.plist" 2>/dev/null)"
}

for app in "${apps[@]}"; do report "$app"; done

echo "================ copies macOS knows about"
for id in uk.co.riera.castellum uk.co.riera.castellum.dev; do
  n="$(mdfind "kMDItemCFBundleIdentifier == '$id'" 2>/dev/null | wc -l | tr -d ' ')"
  echo "  Spotlight $id: $n bundle(s)"
  mdfind "kMDItemCFBundleIdentifier == '$id'" 2>/dev/null \
    | sed -E 's#DerivedData/Castellum-[a-z]+/#DerivedData/Castellum-*/#; s#worktrees/[^/]+/#worktrees/*/#' \
    | sort | uniq -c | sed 's/^/    /'
done
dd_n="$(find "$HOME/Library/Developer/Xcode/DerivedData" -maxdepth 1 -name 'Castellum-*' 2>/dev/null | wc -l | tr -d ' ')"
echo "  ~/Library/Developer/Xcode/DerivedData/Castellum-*: $dd_n (canonical builds live in build/DerivedData.noindex; clean with scripts/clean-castellum-derived-data.sh)"
echo "  this checkout's build config default: Debug (scripts/build-macos.sh)"
echo "  profile dirs (S68): Release = ~/Library/Application Support/CastellumNative/profile; Debug default = .../CastellumNativeDev/profile; CASTELLUM_PROFILE_DIR wins (existence only, contents never read):"
for d in CastellumNative CastellumNativeDev; do
  [[ -d "$HOME/Library/Application Support/$d" ]] && echo "    $d: present" || echo "    $d: absent"
done

if ((tcc_log)); then
  echo "================ tccd prompts for Castellum, last 24h (read-only log show)"
  /usr/bin/log show --last 24h --style compact \
    --predicate 'process == "tccd" AND eventMessage CONTAINS "AUTHREQ_PROMPTING" AND eventMessage CONTAINS[c] "castellum"' 2>/dev/null \
    | grep -oE 'service=kTCCService[A-Za-z]+|binary_path=[^}]*' | paste - - \
    | sed -E 's#DerivedData/Castellum-[a-z]+/#DerivedData/Castellum-*/#' | sort | uniq -c | sed 's/^/  /'
  echo "  (SystemPolicyAppData = 'access data from other apps'; AppleEvents = Automation)"
fi
