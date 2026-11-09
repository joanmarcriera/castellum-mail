#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Package a built Castellum.app into a distributable DMG.
#
#   bash apple/scripts/make-dmg.sh <path/to/Castellum.app> <out.dmg>
#
# Deliberately plain `hdiutil`: no background image or window geometry, so
# there is no AppleScript/Finder step to hang a headless runner. The DMG
# contains the app plus an /Applications symlink, which is the drag-to-install
# convention users expect.
set -euo pipefail

APP="${1:?usage: make-dmg.sh <App.app> <out.dmg>}"
OUT="${2:?usage: make-dmg.sh <App.app> <out.dmg>}"

[[ -d "$APP" ]] || { echo "make-dmg: no such app bundle: $APP" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

rm -f "$OUT"
hdiutil create \
  -volname "Castellum" \
  -srcfolder "$STAGE" \
  -ov -format UDZO \
  "$OUT"

echo "make-dmg: wrote $OUT"
