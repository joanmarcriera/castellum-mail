# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xi scenario builder helpers. Usage: from gen import *; write('xi-foo', covers, steps)"""
import json, os
ROOT = os.path.dirname(os.path.abspath(__file__)) + "/../../.."
def acct(local="xi-r1", ref="xi"): return {"op":"addAccount","email":f"{local}@local.test","password":f"{local}@local.test","as":ref,"waitFor":"synced","timeoutMs":90000}
def persona(p): return {"op":"addAccount","persona":p,"as":p,"waitFor":"synced","timeoutMs":90000}
def rd(id, **k): return {"op":"reading","id":id, **k}
idle = {"op":"wait","until":"idle"}
def inbox(ref="xi"): return [rd("selectMailbox", accountRef=ref, text="inbox"), idle]
def sel(t): return [rd("selectMessage", text=t), idle]
def cap(n, windows=None):
    d = {"op":"capture","name":n}
    if windows: d["windows"] = windows
    return d
def exp(pairs, window=None, rules=None):
    d = {"op":"assert","expect":[{"model":k,"equals":v} for k,v in pairs]}
    if window: d["window"] = window; d["rules"] = rules or ["nonBlank"]
    return d
def write(name, covers, steps, personas=False, screen=(1280,768)):
    def fl(x):
        return sum((fl(i) for i in x), []) if isinstance(x, list) else [x]
    flat = fl(steps)
    sc = {"schema":1,"name":name,"covers":covers,"requires":{"greenmail":True,"personas":personas},
          "screen":{"width":screen[0],"height":screen[1],"menuBar":25},"timeoutMs":240000,
          "steps":[{"op":"wait","until":"accountsLoaded"}] + flat + [{"op":"quit"}]}
    p = f"{ROOT}/scripts/ui-scenarios/{name}.json"
    json.dump(sc, open(p,"w"), indent=1, ensure_ascii=False); print("wrote", os.path.normpath(p))
