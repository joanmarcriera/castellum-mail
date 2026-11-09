#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xa explorer, third batch (one defect scenario). Usage: python3 build/explore/xa/gen3.py (from the worktree root)."""
import json
S = "sheet:addAccount"; ONE, TWO = "xa-one@local.test", "xa-two@local.test"
idle = {"op": "wait", "until": "idle"}
RULES = ["fitsScreen", "primaryButtonsVisible", "noClippedText", "noZeroSizeTextFields"]
def st(i, t): return {"op": "setText", "window": S, "find": {"identifier": i}, "text": t}
def layout(label): return {"op": "assert", "label": label, "window": S, "rules": RULES,
                           "expect": [{"find": {"identifier": "cancel"}, "visible": True}, {"find": {"identifier": "sync"}, "visible": True}]}
steps = [{"op": "wait", "until": "accountsLoaded"}, {"op": "resizeWindow", "window": "main", "contentSize": [1200, 690]}, idle,
  {"op": "addAccount", "email": ONE, "password": ONE, "as": "one", "waitFor": "synced", "timeoutMs": 90000}, idle,
  {"op": "openSheet", "target": "addAccount"}, {"op": "wait", "until": S}, idle,
  st("email", TWO), st("imapHost", "127.0.0.1"), st("password", TWO), idle, {"op": "capture", "name": "01-new-address-control", "windows": [S]},
  layout("control: a new address lays out inside the sheet"),
  st("email", ONE), idle, {"op": "capture", "name": "02-existing-address-warning", "windows": [S]},
  layout("the 'already exists' warning must not push the form outside the sheet"),
  {"op": "press", "window": S, "find": {"identifier": "cancel"}}, {"op": "wait", "until": "noSheet:addAccount"}, {"op": "quit"}]
json.dump({"schema": 1, "name": "xa-defect-duplicate-warning-clips-sheet",
  "covers": ["Add sheet with an account already present: typing an address that already exists shows a warning; the form must still fit the 420-point sheet (labels, fields, Cancel and Sync inside it)"],
  "requires": {"greenmail": True, "personas": False}, "screen": {"width": 1280, "height": 768, "menuBar": 25}, "timeoutMs": 180000, "steps": steps},
  open("scripts/ui-scenarios/xa-defect-duplicate-warning-clips-sheet.json", "w"), indent=1)
print("ok")
