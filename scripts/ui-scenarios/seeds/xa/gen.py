#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xa explorer: generates scripts/ui-scenarios/xa-*.json (creates files only).
Usage: python3 build/explore/xa/gen.py   (from the worktree root)
Sheet coordinates assume main content 1200x690 (first-account sheet is then 420x650)."""
import json
S = "sheet:addAccount"
def scen(name, covers, steps, timeout=180000, gm=True):
    d = {"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": gm, "personas": False},
         "screen": {"width": 1280, "height": 768, "menuBar": 25}, "timeoutMs": timeout, "steps": steps + [{"op": "quit"}]}
    json.dump(d, open(f"scripts/ui-scenarios/{name}.json", "w"), indent=1)
start = [{"op": "wait", "until": "accountsLoaded"},
         {"op": "resizeWindow", "window": "main", "contentSize": [1200, 690]}, {"op": "wait", "until": "idle"}]
opensheet = [{"op": "openSheet", "target": "addAccount"}, {"op": "wait", "until": S}, {"op": "wait", "until": "idle"}]
def st(i, t): return {"op": "setText", "window": S, "find": {"identifier": i}, "text": t}
def nth(n, t): return {"op": "accounts", "id": "setNth", "window": S, "name": str(n), "text": t}
def settle(ms): return {"op": "accounts", "id": "settle", "text": str(ms)}
def cap(n, w=None):
    c = {"op": "capture", "name": n}
    if w: c["windows"] = w
    return c
idle = {"op": "wait", "until": "idle"}
tls_off = [{"op": "accounts", "id": "click", "window": S, "text": "179,270"}, {"op": "accounts", "id": "click", "window": S, "text": "179,442"}]
def fill(email, pw, host="127.0.0.1", port="3143", user=None, tls=True):
    out = []
    if email is not None: out.append(st("email", email))
    out += [st("imapHost", host), nth(3, port)]
    if user: out.append(st("username", user))
    out += [st("password", pw), nth(7, "3025")]
    if tls: out += tls_off
    return out + [idle]
press_sync = {"op": "press", "window": S, "find": {"identifier": "sync"}}
cancel = [{"op": "press", "window": S, "find": {"identifier": "cancel"}}, {"op": "wait", "until": "noSheet:addAccount"}, idle]
def sync_enabled(v): return {"op": "assert", "window": S, "expect": [{"find": {"identifier": "sync"}, "enabled": v}]}
def model(*kv): return {"op": "assert", "expect": [{"model": k, "equals": v} for k, v in kv]}
ONE, TWO, CAPS = "xa-one@local.test", "xa-two@local.test", "xa-caps@local.test"
def add(email, ref, wait="synced"): return {"op": "addAccount", "email": email, "password": email.strip().lower(), "as": ref, "waitFor": wait, "timeoutMs": 90000}

# ---------------- defects (each must FAIL) ----------------
scen("xa-defect-first-account-nothing-selected",
     ["First account added through the sheet: once the sheet closes the message list must show the new account's mail (Inbox or All Inboxes selected), not an empty list"],
     start + opensheet + fill(ONE, ONE) + [press_sync,
      {"op": "wait", "until": f"sync:{ONE}=ok", "timeoutMs": 60000}, {"op": "wait", "until": "noSheet:addAccount", "timeoutMs": 15000}, settle(1500), idle,
      cap("01-main-after-first-account", ["main"]),
      model(("accountCount", "1"), ("reading:hasMessages", "true"))])
scen("xa-defect-duplicate-by-capitals-or-space",
     ["Adding the same mailbox again with different capitals or a trailing space must be refused like the exact duplicate is (one mailbox = one account)"],
     start + [add(ONE, "one"), idle] + opensheet + [st("email", ONE), st("imapHost", "127.0.0.1"), st("password", ONE), idle, sync_enabled(False),
      st("email", "XA-One@Local.Test"), idle, cap("01-capitals", [S]), sync_enabled(False),
      st("email", ONE + " "), idle, cap("02-trailing-space", [S]), sync_enabled(False)] + cancel +
     [add("XA-One@Local.Test", "caps", "none"), add(ONE + " ", "space", "none"), settle(8000), idle,
      {"op": "wait", "until": "settingsOpenerReady"}, {"op": "openWindow", "target": "settings", "tab": "accounts"}, {"op": "wait", "until": "window:settings"},
      {"op": "resizeWindow", "window": "settings", "contentSize": [760, 560]}, idle, cap("03-settings-three-copies", ["settings"]),
      model(("accountCount", "1"))], timeout=220000)
scen("xa-defect-blank-email-account",
     ["Add sheet with the Email field empty (username filled) must not offer Sync: it creates an account with no address, no sidebar name and an empty From"],
     start + opensheet + fill(None, TWO, user=TWO) + [cap("01-blank-email-filled", [S]), sync_enabled(False), press_sync, settle(6000), idle,
      {"op": "wait", "until": "settingsOpenerReady"}, {"op": "openWindow", "target": "settings", "tab": "accounts"}, {"op": "wait", "until": "window:settings"},
      {"op": "resizeWindow", "window": "settings", "contentSize": [760, 560]}, idle, cap("02-settings-nameless-account", ["settings"]),
      model(("accountCount", "0"))])
scen("xa-defect-port-out-of-range",
     ["A port outside 1-65535 must not be accepted by the Add sheet (today Sync is enabled and the failure is a raw 'CastellumError error 2')"],
     start + opensheet + fill(ONE, ONE, port="99999") + [sync_enabled(False), press_sync, settle(4000), cap("01-port-99999", [S]),
      {"op": "assert", "window": S, "expect": [{"find": {"text": "CastellumError"}, "exists": False}]}] + cancel + [model(("accountCount", "0"))], gm=False)
scen("xa-defect-first-run-window-wider-than-1280",
     ["First run: the main window and its minimum size must fit a 1280-point-wide display"],
     [{"op": "wait", "until": "accountsLoaded"}, idle, cap("01-first-run-window", ["main"]),
      {"op": "assert", "window": "main", "rules": ["fitsScreen"]}], timeout=60000, gm=False)

# ---------------- coverage (each must PASS) ----------------
scen("xa-first-run-empty",
     ["First run, empty profile: main window, Settings > Accounts empty state, Status dashboard, Outbox, compose refused with a 'Can't Compose' alert that Cancel dismisses"],
     [{"op": "wait", "until": "accountsLoaded"}, idle, cap("01-main-first-run", ["main"]),
      {"op": "assert", "window": "main", "rules": ["nonBlank"], "expect": [{"model": "accountCount", "equals": "0"}, {"model": "errorText", "equals": ""}, {"sheetAttached": False}, {"model": "reading:syncBanner", "equals": "none"}]},
      {"op": "wait", "until": "settingsOpenerReady"}, {"op": "openWindow", "target": "settings", "tab": "accounts"}, {"op": "wait", "until": "window:settings"},
      {"op": "resizeWindow", "window": "settings", "contentSize": [760, 560]}, idle, cap("02-settings-accounts-empty", ["settings"]),
      {"op": "assert", "window": "settings", "rules": ["nonBlank", "fitsScreen"]}, {"op": "closeWindow", "window": "settings"},
      {"op": "openWindow", "target": "status"}, {"op": "wait", "until": "window:status"}, idle, cap("03-status-empty", ["status"]),
      {"op": "assert", "window": "status", "rules": ["nonBlank", "fitsScreen", "noClippedText"]}, {"op": "closeWindow", "window": "status"},
      {"op": "openWindow", "target": "compose"}, {"op": "wait", "until": "window:compose"}, idle, cap("04-compose-refused-alert"),
      {"op": "press", "window": "compose", "find": {"role": "button", "label": "Cancel"}}, settle(600), idle,
      model(("accounts:windowTitles", "Castellum"), ("errorText", ""), ("accountCount", "0"))], timeout=90000, gm=False)
scen("xa-add-first-account-sheet",
     ["First account through the real Add sheet (TLS off, GreenMail): Sync enabled only when filled, sheet closes by itself, account is synced, stored endpoint is right, no error alert"],
     start + opensheet + [cap("01-blank-sheet", [S]),
      {"op": "assert", "window": S, "rules": ["nonBlank", "fitsScreen", "primaryButtonsVisible", "noClippedText", "noZeroSizeTextFields"],
       "expect": [{"find": {"identifier": "sync"}, "enabled": False}, {"find": {"identifier": "testConnection"}, "enabled": False}]},
      st("email", ONE), idle, sync_enabled(False)] + fill(ONE, ONE) + [cap("02-filled", [S]), sync_enabled(True), press_sync,
      {"op": "wait", "until": f"sync:{ONE}=ok", "timeoutMs": 60000}, {"op": "wait", "until": "noSheet:addAccount", "timeoutMs": 15000}, idle, cap("03-main-after", ["main"]),
      model(("accountCount", "1"), ("accounts:dotState:" + ONE, "ok"), ("accounts:endpoint:" + ONE, "127.0.0.1:3143"),
            ("accounts:sidebarLabel:" + ONE, ONE), ("errorText", ""), ("reading:syncBanner", "none")),
      {"op": "reading", "id": "selectMailbox", "accountRef": ONE, "text": "inbox"}, idle, cap("04-inbox-selected", ["main"]),
      model(("reading:hasMessages", "true"))])
scen("xa-wrong-password-then-fix",
     ["Wrong password in the Add sheet: inline sign-in failure, nothing saved, fields kept; correcting the password and pressing Sync again adds the account"],
     start + opensheet + fill(ONE, "definitely-wrong") + [press_sync,
      {"op": "accounts", "id": "waitNode", "window": S, "find": {"text": "IMAP sign-in to 127.0.0.1:3143 failed"}, "value": True, "timeoutMs": 20000},
      cap("01-wrong-password", [S]),
      {"op": "assert", "window": S, "rules": ["primaryButtonsVisible"], "expect": [{"find": {"identifier": "cancel"}, "visible": True}, {"find": {"identifier": "sync"}, "enabled": True},
        {"model": "accountCount", "equals": "0"}, {"model": f"accounts:field:{S}|#0", "equals": ONE}, {"model": f"accounts:field:{S}|#2", "equals": "127.0.0.1"}]},
      st("password", ONE), idle, press_sync, {"op": "wait", "until": f"sync:{ONE}=ok", "timeoutMs": 60000},
      {"op": "wait", "until": "noSheet:addAccount", "timeoutMs": 15000}, idle, model(("accountCount", "1"), ("errorText", ""))])
scen("xa-unreachable-then-cancel",
     ["Unreachable port and unknown host in the Add sheet: human-readable inline errors per leg, then Cancel leaves no account, no status dot, no banner, no alert"],
     start + opensheet + fill(ONE, ONE, port="3999") + [press_sync,
      {"op": "accounts", "id": "waitNode", "window": S, "find": {"text": "Can’t reach 127.0.0.1:3999 (IMAP)"}, "value": True, "timeoutMs": 20000},
      cap("01-unreachable-port", [S]), st("imapHost", "no-such-host.invalid"), nth(3, "3143"), idle, press_sync,
      {"op": "accounts", "id": "waitNode", "window": S, "find": {"text": "Can’t reach no-such-host.invalid:3143 (IMAP)"}, "value": True, "timeoutMs": 30000},
      cap("02-unknown-host", [S]), nth(3, "abc"), idle,
      {"op": "assert", "window": S, "expect": [{"find": {"identifier": "sync"}, "enabled": False}, {"find": {"identifier": "testConnection"}, "enabled": False}]}] + cancel +
     [cap("03-main-after-cancel", ["main"]),
      model(("accountCount", "0"), ("reading:syncBanner", "none"), ("accounts:dotState:" + ONE, "none"), ("errorText", "")), {"op": "assert", "expect": [{"sheetAttached": False}]}])
scen("xa-default-tls-on-plain-port",
     ["Add sheet defaults (TLS on, SMTP 587) against a plain-text server: the failure names the TLS setting, nothing is saved"],
     start + opensheet + [st("email", ONE), st("imapHost", "127.0.0.1"), nth(3, "3143"), st("password", ONE), idle, press_sync,
      {"op": "accounts", "id": "waitNode", "window": S, "find": {"text": "check the port and TLS setting"}, "value": True, "timeoutMs": 30000},
      cap("01-tls-mismatch", [S]), {"op": "assert", "window": S, "rules": ["primaryButtonsVisible"], "expect": [{"model": "accountCount", "equals": "0"}]}] + cancel + [model(("accountCount", "0"), ("errorText", ""))])
scen("xa-same-email-blocked",
     ["Adding the exact same address a second time: Sync is disabled, Cancel leaves one account"],
     start + [add(ONE, "one"), idle] + opensheet + [st("email", ONE), st("imapHost", "127.0.0.1"), st("password", ONE), idle, cap("01-same-email", [S]), sync_enabled(False),
      st("email", TWO), idle, sync_enabled(True)] + cancel + [model(("accountCount", "1"), ("accounts:dotState:one", "ok"))])
scen("xa-three-accounts",
     ["Second and third account: all three sync, All Inboxes lists their mail, Settings > Accounts and the Status dashboard list three rows without clipping"],
     start + [add(ONE, "one"), add(TWO, "two"), add(CAPS, "caps"), idle, {"op": "reading", "id": "selectUnified", "text": "inbox"}, idle, cap("01-all-inboxes", ["main"]),
      model(("accountCount", "3"), ("accounts:dotState:one", "ok"), ("accounts:dotState:two", "ok"), ("accounts:dotState:caps", "ok"), ("reading:isUnified", "true"), ("reading:hasMessages", "true")),
      {"op": "wait", "until": "settingsOpenerReady"}, {"op": "openWindow", "target": "settings", "tab": "accounts", "accountRef": "two"}, {"op": "wait", "until": "window:settings"},
      {"op": "resizeWindow", "window": "settings", "contentSize": [760, 560]}, idle, cap("02-settings-three", ["settings"]),
      {"op": "assert", "window": "settings", "rules": ["nonBlank", "fitsScreen"]},
      {"op": "openWindow", "target": "status"}, {"op": "wait", "until": "window:status"}, {"op": "resizeWindow", "window": "status", "contentSize": [620, 460]}, idle, cap("03-status-three", ["status"]),
      {"op": "assert", "window": "status", "rules": ["nonBlank", "fitsScreen", "noClippedText"]}], timeout=240000)
print("ok")
