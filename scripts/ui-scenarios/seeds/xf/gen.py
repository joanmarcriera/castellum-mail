#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer: scenario builder helpers. Usage: `from gen import *` in a small script, then write(name, covers, steps).
Never overwrites a file it did not create (names always start with xf-)."""
import json, os
ROOT = os.path.dirname(os.path.abspath(__file__)) + "/../../.."
def W(u, t=None): d = {"op": "wait", "until": u}; d.update({"timeoutMs": t} if t else {}); return d
IDLE = W("idle")
def add(tag, t=90000): return {"op": "addAccount", "email": f"xf-{tag}@local.test", "password": f"xf-{tag}@local.test", "as": tag, "waitFor": "synced", "timeoutMs": t}
def R(id, **k): d = {"op": "reading", "id": id}; d.update(k); return d
def X(id, **k): d = {"op": "ext", "id": id}; d.update(k); return d
def A(id, **k): d = {"op": "accounts", "id": id}; d.update(k); return d
def mbox(ref, role): return R("selectMailbox", accountRef=ref, text=role)
def msg(s): return R("selectMessage", text=s)
def cmd(id): return {"op": "command", "id": id, "allowServerWrites": True}
def cap(n): return {"op": "capture", "name": n}
def settle(ms): return A("settle", text=str(ms))
def exp(**kv):
    out = []
    for k, v in kv.items(): out.append({"model": k.replace("__", ":").replace("_", ":", 1) if False else k, "equals": v})
    return {"op": "assert", "expect": out}
def E(*pairs): return {"op": "assert", "expect": [{"model": k, "equals": v} for k, v in pairs]}
def waitR(key, val, t=20000): return R("waitFor", name=key, text=val, timeoutMs=t)
def write(name, covers, steps, timeout=240000):
    assert name.startswith("xf-")
    s = {"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": True},
         "screen": {"width": 1280, "height": 768, "menuBar": 25}, "timeoutMs": timeout,
         "steps": [W("accountsLoaded")] + steps + [{"op": "quit"}]}
    p = f"{ROOT}/scripts/ui-scenarios/{name}.json"
    json.dump(s, open(p, "w"), indent=1); print("wrote", os.path.normpath(p))
def reg(label, at="1000,330"): return A("click", window="main", text=at, name=label)
def sheet(v): return {"op": "assert", "expect": [{"sheetAttached": v}]}
def capall(n): return {"op": "capture", "name": n, "windows": ["all"]}
def press(win, text): return {"op": "press", "window": win, "find": {"text": text}}
def refused(verb_cmd, label, button="Keep Read-only"):
    """register label, run command, expect the blocked alert, capture, dismiss with `button`."""
    return [reg("sheet:" + label), cmd(verb_cmd), IDLE, settle(700), sheet(True), capall(label), press("sheet:" + label, button), settle(500), sheet(False)]
def dump(*keys, win=True):
    e = [{"model": k, "equals": "?"} for k in keys]
    if win: e += [{"windowCount": 99}]
    return {"op": "assert", "expect": e}
def writes(tag): return X("enableWrites", email=f"xf-{tag}@local.test")
def drained(t=40000): return X("waitExt", name="pendingCount", text="0", timeoutMs=t)
def run(n="xf-tmp-probe"): pass
