#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer: compact scenario writer. Usage: import mk; mk.write(name, covers, steps, timeout).
Step shorthands return dicts in the UIT-1 schema (scripts/ui-scenarios/*.json)."""
import json, os
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../.."))
ACCT = "xj-org1@local.test"
def R(id, **k): return {"op": "reading", "id": id, **k}
def A(id, **k): return {"op": "accounts", "id": id, **k}
def E(id, **k): return {"op": "ext", "id": id, **k}
idle = {"op": "wait", "until": "idle"}
def cap(name, windows=None):
    d = {"op": "capture", "name": name}
    if windows: d["windows"] = windows
    return d
def exp(pairs, window=None, rules=None, finds=None):
    d = {"op": "assert", "expect": [{"model": k, "equals": v} for k, v in pairs.items()]}
    if window: d["window"] = window
    if rules: d["rules"] = rules
    for f in finds or []: d["expect"].append(f)
    return d
def start(email=ACCT, ref="xj"):
    return [{"op": "wait", "until": "accountsLoaded"},
            {"op": "addAccount", "email": email, "password": email, "as": ref, "waitFor": "synced", "timeoutMs": 90000}]
def persona(p): return {"op": "addAccount", "persona": p, "as": p, "waitFor": "synced", "timeoutMs": 90000}
def inbox(ref="xj"): return [R("selectMailbox", accountRef=ref, text="inbox"), idle]
def sel(subject): return [R("selectMessage", text=subject), idle]
def wf(key, val, t=15000): return R("waitFor", name=key, text=val, timeoutMs=t)
def write(name, covers, steps, timeout=180000, screen=(1280, 768)):
    def fl(x): return [y for i in x for y in fl(i)] if isinstance(x, list) else [x]
    flat = fl(steps)
    flat.append({"op": "quit"})
    sc = {"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": True, "personas": True},
          "screen": {"width": screen[0], "height": screen[1], "menuBar": 25}, "timeoutMs": timeout, "steps": flat}
    p = os.path.join(ROOT, "scripts/ui-scenarios", name + ".json")
    json.dump(sc, open(p, "w"), indent=1); print("wrote", p)
