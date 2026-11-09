#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xa explorer, second batch: generates scripts/ui-scenarios/xa-*.json (xa-prefixed files only).
Usage: python3 build/explore/xa/gen2.py   (from the worktree root)
Settings coordinates assume the Settings window content is 760x560 (Remove... at 708,538)."""
import json
S = "sheet:addAccount"
def scen(name, covers, steps, timeout=180000, gm=True, personas=False):
    d = {"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": gm, "personas": personas},
         "screen": {"width": 1280, "height": 768, "menuBar": 25}, "timeoutMs": timeout, "steps": steps + [{"op": "quit"}]}
    json.dump(d, open(f"scripts/ui-scenarios/{name}.json", "w"), indent=1)
idle = {"op": "wait", "until": "idle"}
start = [{"op": "wait", "until": "accountsLoaded"}, idle]
def settle(ms): return {"op": "accounts", "id": "settle", "text": str(ms)}
def cap(n, w=None):
    c = {"op": "capture", "name": n}
    if w: c["windows"] = w
    return c
def model(*kv): return {"op": "assert", "expect": [{"model": k, "equals": v} for k, v in kv]}
def add(email, ref, wait="synced"): return {"op": "addAccount", "email": email, "password": email, "as": ref, "waitFor": wait, "timeoutMs": 90000}
ONE, TWO = "xa-one@local.test", "xa-two@local.test"
def settings(ref):
    o = {"op": "openWindow", "target": "settings", "tab": "accounts"}
    if ref: o["accountRef"] = ref
    return [{"op": "wait", "until": "settingsOpenerReady"}, o, {"op": "wait", "until": "window:settings"},
            {"op": "resizeWindow", "window": "settings", "contentSize": [760, 560]}, idle]
remove = [{"op": "accounts", "id": "click", "window": "settings", "text": "708,538", "name": "sheet:remove"},
          {"op": "wait", "until": "sheet:remove", "timeoutMs": 5000}, idle]
confirm = [{"op": "press", "window": "sheet:remove", "find": {"role": "button", "label": "Remove"}},
           {"op": "wait", "until": "noSheet:remove", "timeoutMs": 5000}]

scen("xa-first-run-empty",
     ["First run, empty profile: main window, Settings > Accounts empty state, Status dashboard"],
     start + [cap("01-main-first-run", ["main"]),
      {"op": "assert", "window": "main", "rules": ["nonBlank"], "expect": [{"model": "accountCount", "equals": "0"}, {"model": "errorText", "equals": ""}, {"sheetAttached": False}, {"model": "reading:syncBanner", "equals": "none"}]}]
     + settings(None) + [cap("02-settings-accounts-empty", ["settings"]),
      {"op": "assert", "window": "settings", "rules": ["nonBlank", "fitsScreen"]}, {"op": "closeWindow", "window": "settings"},
      {"op": "openWindow", "target": "status"}, {"op": "wait", "until": "window:status"}, idle, cap("03-status-empty", ["status"]),
      {"op": "assert", "window": "status", "rules": ["nonBlank", "fitsScreen", "noClippedText"]}, {"op": "closeWindow", "window": "status"},
      model(("accountCount", "0"), ("errorText", ""))], timeout=90000, gm=False)

# The two remove scenarios below cannot run: a synthetic click never raises the SwiftUI Remove/Reset alert (harness gap).
if False:
    scen("xa-defect-remove-account-leaves-its-mail-on-screen",
         ["Removing an account while All Inboxes shows its mail: the removed account's messages must leave the list and the open message must close"],
         start + [add(ONE, "one"), add(TWO, "two"), idle, {"op": "reading", "id": "selectUnified", "text": "inbox"}, idle,
          {"op": "reading", "id": "selectMessage", "text": "XA two only message"}, idle, cap("01-unified-before", ["main"]),
          model(("reading:selectedSubject", "XA two only message"), ("reading:messageCount", "8"))] + settings("two") + remove + [cap("02-remove-confirm", ["sheet:remove", "settings"])] + confirm +
         [{"op": "reading", "id": "waitFor", "name": "accountCount", "text": "1", "timeoutMs": 20000}, settle(2500), idle,
          cap("03-after-remove", ["main", "settings"]),
          {"op": "assert", "expect": [{"model": "accountCount", "equals": "1"}, {"model": "accounts:dotState:two", "equals": "none"},
            {"model": "reading:selectedSubject", "equals": "none"}, {"model": "reading:messageCount", "equals": "6"}]}], timeout=240000)

    scen("xa-remove-own-mailbox-then-readd",
         ["Removing the only account while its Inbox is selected empties the list and reader, leaves no status dot, banner or error; adding it again syncs it back"],
         start + [add(ONE, "one"), idle, {"op": "reading", "id": "selectMailbox", "accountRef": "one", "text": "inbox"}, idle,
          {"op": "reading", "id": "selectMessage", "text": "XA one message 1"}, idle, model(("reading:hasMessages", "true"))] + settings("one") + remove +
         [cap("01-remove-confirm", ["sheet:remove"]),
          {"op": "assert", "window": "sheet:remove", "rules": ["nonBlank", "fitsScreen", "primaryButtonsVisible"], "expect": [{"find": {"role": "button", "label": "Cancel"}, "visible": True}]}] + confirm +
         [{"op": "reading", "id": "waitFor", "name": "accountCount", "text": "0", "timeoutMs": 20000}, settle(1500), idle, cap("02-after-remove", ["main", "settings"]),
          model(("accountCount", "0"), ("reading:hasMessages", "false"), ("reading:selectedSubject", "none"), ("reading:syncBanner", "none"),
                ("errorText", ""), ("accounts:dotState:one", "none")),
          add(ONE, "one"), idle, {"op": "reading", "id": "selectMailbox", "accountRef": "one", "text": "inbox"}, idle, cap("03-readded", ["main"]),
          model(("accountCount", "1"), ("accounts:dotState:one", "ok"), ("reading:hasMessages", "true"), ("errorText", ""))], timeout=240000)

scen("xa-empty-mailbox-account",
     ["An account whose mailbox is empty (xa-empty, emptied over IMAP): first sync ends ok, Inbox selected shows an empty list with no error, banner or alert; Settings row shows it synced"],
     start + [add("xa-empty@local.test", "dave"), idle,
      {"op": "reading", "id": "selectMailbox", "accountRef": "dave", "text": "inbox"}, idle, cap("01-dave-inbox", ["main"]),
      model(("accountCount", "1"), ("accounts:dotState:dave", "ok"), ("reading:mailboxRole", "inbox"), ("reading:hasMessages", "false"),
            ("reading:syncBanner", "none"), ("errorText", "")), {"op": "assert", "expect": [{"sheetAttached": False}]}]
     + settings("dave") + [cap("02-settings-dave", ["settings"]), {"op": "assert", "window": "settings", "rules": ["nonBlank", "fitsScreen"]}])

scen("xa-add-sheet-on-smallest-window",
     ["Add-account sheet opened on the main window at its minimum size: the sheet stays on screen with Cancel, Test Connection and Sync reachable"],
     start + [{"op": "resizeWindow", "window": "main", "contentSize": "min"}, idle,
      {"op": "openSheet", "target": "addAccount"}, {"op": "wait", "until": S}, idle, cap("01-sheet-on-min-window"),
      {"op": "assert", "window": S, "rules": ["nonBlank", "fitsScreen", "primaryButtonsVisible", "noZeroSizeTextFields"],
       "expect": [{"find": {"identifier": "cancel"}, "visible": True}, {"find": {"identifier": "sync"}, "visible": True}]},
      {"op": "press", "window": S, "find": {"identifier": "cancel"}}, {"op": "wait", "until": "noSheet:addAccount"},
      model(("accountCount", "0"))], timeout=90000, gm=False)

scen("xa-first-sync-in-progress",
     ["While a first sync runs (account added, not finished): no error text or failure banner appears, and the account ends ok"],
     start + [{"op": "addAccount", "persona": "alice", "as": "alice", "waitFor": "none"}, settle(300), cap("01-during-first-sync", ["main"]),
      {"op": "wait", "until": "sync:alice=ok", "timeoutMs": 120000}, idle, cap("03-after-first-sync", ["main"]),
      model(("accountCount", "1"), ("accounts:dotState:alice", "ok"), ("errorText", ""), ("reading:syncBanner", "none"))], timeout=240000, personas=True)
print("ok")
