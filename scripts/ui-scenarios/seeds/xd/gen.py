# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xd scenario helpers: compact DSL -> scripts/ui-scenarios/xd-*.json"""
import json, os
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../.."))
def W(u="idle"): return {"op": "wait", "until": u}
def R(id, **k): return {"op": "reading", "id": id, **k}
def S(t): return [{"op": "search", "text": t}, W()]
def A(*pairs, window=None, rules=None):
    d = {"op": "assert", "expect": [{"model": k, "equals": v} for k, v in pairs]}
    if window: d["window"] = window
    if rules: d["rules"] = rules
    return d
def P(*keys): return A(*[("reading:" + k, "?") for k in keys])   # probe: prints actual values
def C(n, windows=None):
    d = {"op": "capture", "name": n}
    if windows: d["windows"] = windows
    return d
def XD(): return {"op": "addAccount", "email": "xd-s1@local.test", "password": "xd-s1@local.test", "as": "xd", "waitFor": "synced", "timeoutMs": 90000}
def PERS(p): return {"op": "addAccount", "persona": p, "as": p, "waitFor": "synced", "timeoutMs": 90000}
def flat(steps):
    out = []
    for s in steps:
        if isinstance(s, list): out += flat(s)
        else: out.append(s)
    return out
def write(name, covers, steps, timeout=240000, screen=(1280, 768), d=None):
    sc = {"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": True, "personas": True},
          "screen": {"width": screen[0], "height": screen[1], "menuBar": 25}, "timeoutMs": timeout,
          "steps": flat([W("accountsLoaded")] + steps + [{"op": "quit"}])}
    p = d or os.path.join(ROOT, "scripts/ui-scenarios")
    with open(os.path.join(p, name + ".json"), "w") as f: json.dump(sc, f, indent=1, ensure_ascii=False)
    print("wrote", name)
