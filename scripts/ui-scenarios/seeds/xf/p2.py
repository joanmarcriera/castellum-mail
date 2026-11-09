# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2 probes. Usage: python3 p2.py  (writes build/explore/xf/probes/*.json; run by path)"""
import json, os
from gen import *
import gen
def wprobe(name, steps, timeout=240000):
    s = {"schema": 1, "name": name, "covers": ["probe"], "requires": {"greenmail": True},
         "screen": {"width": 1280, "height": 768, "menuBar": 25}, "timeoutMs": timeout,
         "steps": [W("accountsLoaded")] + steps + [{"op": "quit"}]}
    p = os.path.dirname(os.path.abspath(__file__)) + f"/probes/{name}.json"
    json.dump(s, open(p, "w"), indent=1); print("wrote", p)
def key(k): return X("keyEquivalent", window="main", text=k)
D = lambda *k: dump(*k, win=False)
wprobe("xf-tmp-p1", [
    add("m"), writes("m"), mbox("m", "inbox"), IDLE, msg("XF m msg 3"), IDLE, settle(2500), drained(),
    D("reading:selectedSubject"), cmd("archive"), settle(1500), cap("after-archive-selection"),
    D("reading:selectedSubject", "reading:messageCount", "reading:subjects"),
    key("cmd+a"), settle(600), cap("select-all"), cmd("toggleFlag"), settle(800), drained(),
    {"op": "search", "text": "is:flagged"}, IDLE, settle(600), D("reading:messageCount"),
    {"op": "search", "text": ""}, IDLE, settle(600), key("cmd+a"), settle(600), R("deleteSelected"), settle(1200), cap("after-delete-all"),
    D("reading:messageCount", "reading:selectedSubject", "reading:errorText"), drained(), IDLE, settle(1500),
    mbox("m", "trash"), IDLE, settle(1500), D("reading:messageCount", "reading:subjects"),
    R("selectUnified", text="trash"), IDLE, settle(1000), D("reading:messageCount", "reading:isUnified", "reading:mailboxRole"), cap("unified-trash"),
    msg("XF m msg 1"), IDLE, R("deleteSelected"), settle(1200), capall("delete-in-unified-trash"),
    D("reading:pendingDelete", "reading:errorText"),
], timeout=300000)
wprobe("xf-tmp-p2", [
    add("n"), mbox("n", "inbox"), IDLE,
    X("describe", window="main"),
])
