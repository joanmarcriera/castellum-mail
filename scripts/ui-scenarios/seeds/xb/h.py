# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Helpers to generate xb-* UI scenarios (explorer xb). Usage: from h import *; sc(name, covers, steps)."""
import json, os
OUT=os.path.join(os.path.dirname(__file__),"../../../scripts/ui-scenarios")
def sc(name, covers, steps, timeout=180000):
    assert name.startswith("xb-")
    json.dump({"schema":1,"name":name,"covers":covers,"requires":{"greenmail":True,"personas":True},
      "screen":{"width":1280,"height":768,"menuBar":25},"timeoutMs":timeout,"steps":steps}, open(os.path.join(OUT,name+".json"),"w"), indent=1, ensure_ascii=False)
W={"op":"wait","until":"idle"}
def add(p, writes=False, smtp=None):
    a={"op":"addAccount","persona":p,"as":p,"waitFor":"synced","timeoutMs":90000}
    if smtp: a["smtp"]=smtp
    s=[a]
    if writes: s.append({"op":"ext","id":"enableWrites","email":p+"@local.test"})
    return s
def compose(): return [{"op":"openWindow","target":"compose"},{"op":"ext","id":"waitExt","name":"composeWindows","text":"1","timeoutMs":10000},W]
def fld(n,t): return {"op":"ext","id":"setField","window":"@compose","name":n,"text":t}
def body(t): return {"op":"ext","id":"setBody","window":"@compose","text":t}
def pause(ms): return {"op":"ext","id":"pause","text":str(ms)}
def cap(n,w=None):
    d={"op":"capture","name":n}
    if w: d["windows"]=w
    return d
def ex(pairs): return {"op":"assert","expect":[{"model":k,"equals":v} for k,v in pairs]}
def waitx(k,v,t=10000): return {"op":"ext","id":"waitExt","name":k,"text":v,"timeoutMs":t}
def pref(n,v): return {"op":"ext","id":"setPref","name":n,"text":v}
def mbox(p,role="inbox"): return {"op":"ext","id":"selectMailbox","email":p+"@local.test","text":role}
def msg(t,to=120000): return {"op":"ext","id":"selectMessage","text":t,"timeoutMs":to}
close={"op":"ext","id":"closeCompose"}
send={"op":"ext","id":"keyEquivalent","window":"@compose","text":"cmd+return"}
src={"op":"ext","id":"loadSource","timeoutMs":30000}
start=[{"op":"wait","until":"accountsLoaded"}]
Q=[{"op":"quit"}]
