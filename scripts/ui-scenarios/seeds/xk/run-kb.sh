#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# run-kb.sh — run one xk key-bindings scenario with a seeded keybindings.json.
# Usage: build/explore/xk/run-kb.sh <scenario-name> <keybindings.json to seed>
# The harness has no "seed a profile file" op, so this starts ui-check.sh, finds the app
# process launched for THIS scenario (by its CASTELLUM_UI_SCRIPT env), and copies the file
# into that run's temp CASTELLUM_PROFILE_DIR. The scenario pauses 8 s first, then presses
# Settings > General > "Reload Key Bindings". Touches only that temp profile dir.
set -uo pipefail
cd "$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)" || exit 2
name="$1"; seed="$2"
scripts/ui-check.sh --no-build "$name" & chk=$!
for _ in $(seq 1 100); do
  line=$(ps eww -ax -o command 2>/dev/null | grep "CASTELLUM_UI_SCRIPT=[^ ]*/$name.json" | grep -v grep | head -1)
  dir=$(printf '%s\n' "$line" | tr ' ' '\n' | sed -n 's/^CASTELLUM_PROFILE_DIR=//p' | head -1)
  if [[ -n "$dir" && -d "$dir" && "$dir" == *castellum-uit.* ]]; then cp "$seed" "$dir/keybindings.json" && echo "run-kb: seeded $dir/keybindings.json"; break; fi
  sleep 0.2
done
wait "$chk"
