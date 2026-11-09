# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xc scenario generator helpers. Usage: from gen import *; write('xc-foo', covers, steps)"""
import json, os
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
ME = "xc-me@local.test"
def head(writes=False, email=ME):
    s = [{"op": "wait", "until": "accountsLoaded"},
         {"op": "addAccount", "email": email, "password": email, "imap": "127.0.0.1:3143", "smtp": "127.0.0.1:3025", "tls": False, "as": "me", "waitFor": "synced", "timeoutMs": 90000}]
    if writes: s.append({"op": "ext", "id": "enableWrites", "email": email})
    s += [{"op": "reading", "id": "selectMailbox", "accountRef": "me", "text": "inbox"}, {"op": "wait", "until": "idle"}]
    return s
def sel(subject): return [{"op": "reading", "id": "selectMessage", "text": subject}, {"op": "wait", "until": "idle"}]
def reply(cmd="reply"):
    return [{"op": "command", "id": cmd}, {"op": "ext", "id": "waitExt", "name": "composeWindows", "text": "1"}, {"op": "wait", "until": "idle"}]
def cap(name, windows=None):
    s = {"op": "capture", "name": name}
    if windows: s["windows"] = windows
    return [s]
def close(): return [{"op": "ext", "id": "closeCompose"}, {"op": "ext", "id": "waitExt", "name": "composeWindows", "text": "0"}, {"op": "wait", "until": "idle"}]
def expect(*pairs, rules=None, window=None):
    s = {"op": "assert", "expect": [{"model": k, "equals": v} for k, v in pairs]}
    if rules: s["rules"] = rules; s["window"] = window or "@compose"
    return [s]
def write(name, covers, steps, timeout=180000, screen=(1280, 768)):
    sc = {"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": True, "personas": False},
          "screen": {"width": screen[0], "height": screen[1], "menuBar": 25}, "timeoutMs": timeout, "steps": steps + [{"op": "quit"}]}
    p = os.path.join(ROOT, "scripts/ui-scenarios", name + ".json")
    json.dump(sc, open(p, "w"), indent=1, ensure_ascii=False); print(p)
