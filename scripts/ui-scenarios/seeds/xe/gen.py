#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xe explorer: tiny helpers to write scripts/ui-scenarios/xe-*.json (create-only).
Usage: python3 build/explore/xe/gen.py   (rewrites only xe-* scenario files that this script defines)"""
import json, os, sys
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../.."))
R1, R2, SND = "xe-r1@local.test", "xe-r2@local.test", "xe-send@local.test"

def cond(field, value, operator="contains"): return {"field": field, "operator": operator, "value": value}
def group(*conds, op="and"): return {"op": op, "conditions": list(conds), "groups": []}
def rule(name, grp, actions, account=None, stop=False):
    spec = {"group": grp, "actions": actions, "stop": stop}
    if account: spec["account"] = account
    return {"op": "reading", "id": "saveRule", "name": name, "text": json.dumps(spec)}
def add(email, ref, timeout=90000): return {"op": "addAccount", "email": email, "password": email, "as": ref, "waitFor": "synced", "timeoutMs": timeout}
def idle(): return {"op": "wait", "until": "idle"}
def run(name, t=30000): return {"op": "reading", "id": "runRule", "name": name, "timeoutMs": t}
def preview(name): return {"op": "reading", "id": "previewRule", "name": name}
def expect(*pairs, window=None, rules=None):
    s = {"op": "assert", "expect": [{"model": k, "equals": v} for k, v in pairs]}
    if window: s["window"] = window
    if rules: s["rules"] = rules
    return s
def note(v): return ("reading:note", v)
def has(s, v="true"): return ("reading:noteHas:" + s, v)
def cap(name, windows=None):
    s = {"op": "capture", "name": name}
    if windows: s["windows"] = windows
    return s
def writes(ref, on=True): return {"op": "reading", "id": "setWrite", "accountRef": ref, "value": on}
def mailbox(ref, text): return {"op": "reading", "id": "selectMailbox", "accountRef": ref, "text": text}
def waitfor(key, val, t=30000): return {"op": "reading", "id": "waitFor", "name": key, "text": val, "timeoutMs": t}
def waitext(key, val, t=30000): return {"op": "ext", "id": "waitExt", "name": key, "text": val, "timeoutMs": t}
def settle(ms): return {"op": "accounts", "id": "settle", "text": str(ms)}
def rules_tab(): return [{"op": "wait", "until": "settingsOpenerReady"}, {"op": "reading", "id": "settingsTab", "tab": "rules"}, {"op": "wait", "until": "window:settings"}, idle()]
def click(window, x, y, name=None):
    s = {"op": "accounts", "id": "click", "window": window, "text": "%d,%d" % (x, y)}
    if name: s["name"] = name
    return s
START = [{"op": "wait", "until": "accountsLoaded"}]
QUIT = [{"op": "quit"}]
MOVE = lambda mb: {"move": {"mailbox_id": mb}}
STAR = {"add_flag": {}}

def scenario(name, covers, steps, timeout=180000, screen=(1280, 768)):
    d = {"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": True},
         "screen": {"width": screen[0], "height": screen[1], "menuBar": 25}, "timeoutMs": timeout, "steps": steps}
    scratch = name.startswith("xx-")   # exploratory probes stay out of scripts/ui-scenarios
    p = os.path.join(ROOT, "build/explore/xe/scratch" if scratch else "scripts/ui-scenarios", name + ".json")
    os.makedirs(os.path.dirname(p), exist_ok=True)
    assert os.path.basename(p).startswith(("xe-", "xx-"))
    json.dump(d, open(p, "w"), indent=1)
    print("wrote", p)

ONLY = set(sys.argv[1:])
def S(name, covers, steps, **kw):
    if not ONLY or name in ONLY: scenario(name, covers, steps, **kw)

exec(open(os.path.join(os.path.dirname(__file__), "scenarios.py")).read())
